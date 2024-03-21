use std::collections::HashMap;

use crate::quantized_nn::RmsNorm;
use candle::quantized::QTensor;
use candle::quantized::{ggml_file, gguf_file};
use candle::{DType, Device, IndexOp, Result, Tensor, D};
use candle_nn::{Embedding, Module};

pub const MAX_SEQ_LEN: usize = 4096;

// QMatMul wrapper adding some tracing.
#[derive(Debug, Clone)]
struct QMatMul {
    inner: candle::quantized::QMatMul,
    span: tracing::Span,
}

impl QMatMul {
    fn from_qtensor(qtensor: QTensor) -> Result<Self> {
        let inner = candle::quantized::QMatMul::from_qtensor(qtensor)?;
        let span = tracing::span!(tracing::Level::TRACE, "qmatmul");
        Ok(Self { inner, span })
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let _enter = self.span.enter();
        self.inner.forward(xs)
    }
}

#[derive(Debug, Clone)]
struct Mlp {
    feed_forward_w1: QMatMul,
    feed_forward_w2: QMatMul,
    feed_forward_w3: QMatMul,
}

impl Module for Mlp {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let w1 = self.feed_forward_w1.forward(xs)?;
        let w3 = self.feed_forward_w3.forward(xs)?;
        self.feed_forward_w2
            .forward(&(candle_nn::ops::silu(&w1)? * w3)?)
    }
}

#[derive(Debug, Clone)]
enum MlpOrMoe {
    Mlp(Mlp),
    MoE {
        n_expert_used: usize,
        feed_forward_gate_inp: QMatMul,
        experts: Vec<Mlp>,
    },
}

impl Module for MlpOrMoe {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        match self {
            Self::MoE {
                feed_forward_gate_inp,
                experts,
                n_expert_used,
            } => {
                let (b_size, seq_len, hidden_dim) = xs.dims3()?;
                let xs = xs.reshape(((), hidden_dim))?;
                let router_logits = feed_forward_gate_inp.forward(&xs)?;
                let routing_weights = candle_nn::ops::softmax_last_dim(&router_logits)?;

                // In order to extract topk, we extract the data from the tensor and manipulate it
                // directly. Maybe we will want to use some custom ops instead at some point.
                let routing_weights = routing_weights.to_dtype(DType::F32)?.to_vec2::<f32>()?;

                // routing_weights, selected_experts = torch.topk(routing_weights, self.top_k, dim=-1)
                // top_x contains the row indexes to evaluate for each expert.
                let mut top_x = vec![vec![]; experts.len()];
                let mut selected_rws = vec![vec![]; experts.len()];
                for (row_idx, rw) in routing_weights.iter().enumerate() {
                    let mut dst = (0..rw.len() as u32).collect::<Vec<u32>>();
                    dst.sort_by(|&i, &j| rw[j as usize].total_cmp(&rw[i as usize]));
                    let mut sum_routing_weights = 0f32;
                    for &expert_idx in dst.iter().take(*n_expert_used) {
                        let expert_idx = expert_idx as usize;
                        let routing_weight = rw[expert_idx];
                        sum_routing_weights += routing_weight;
                        top_x[expert_idx].push(row_idx as u32);
                    }
                    for &expert_idx in dst.iter().take(*n_expert_used) {
                        let expert_idx = expert_idx as usize;
                        let routing_weight = rw[expert_idx];
                        selected_rws[expert_idx].push(routing_weight / sum_routing_weights)
                    }
                }

                // routing_weights /= routing_weights.sum(dim=-1, keepdim=True)
                // expert_mask = torch.nn.functional.one_hot(selected_experts, num_classes=self.num_experts).permute(2, 1, 0)

                let mut ys = xs.zeros_like()?;
                for (expert_idx, expert_layer) in experts.iter().enumerate() {
                    let top_x = &top_x[expert_idx];
                    if top_x.is_empty() {
                        continue;
                    }
                    let top_x = Tensor::new(top_x.as_slice(), xs.device())?;
                    let selected_rws =
                        Tensor::new(selected_rws[expert_idx].as_slice(), xs.device())?
                            .reshape(((), 1))?;
                    // Index the correct hidden states and compute the expert hidden state for
                    // the current expert. We need to make sure to multiply the output hidden
                    // states by `routing_weights` on the corresponding tokens (top-1 and top-2)
                    let current_state = xs.index_select(&top_x, 0)?.reshape(((), hidden_dim))?;
                    // current_hidden_states = expert_layer(current_state, routing_weights[top_x_list, idx_list, None])
                    let current_hidden_states = expert_layer.forward(&current_state)?;
                    let current_hidden_states =
                        current_hidden_states.broadcast_mul(&selected_rws)?;
                    ys = ys.index_add(&top_x, &current_hidden_states, 0)?;
                }

                let ys = ys.reshape((b_size, seq_len, hidden_dim))?;
                Ok(ys)
            }
            Self::Mlp(mlp) => mlp.forward(xs),
        }
    }
}

#[derive(Debug, Clone)]
struct LayerWeights {
    attention_wq: QMatMul,
    attention_wk: QMatMul,
    attention_wv: QMatMul,
    attention_wo: QMatMul,
    attention_norm: RmsNorm,
    mlp_or_moe: MlpOrMoe,
    ffn_norm: RmsNorm,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    cos: Tensor,
    sin: Tensor,
    neg_inf: Tensor,
    kv_cache: Option<(Tensor, Tensor)>,
    span_attn: tracing::Span,
    span_rot: tracing::Span,
    span_mlp: tracing::Span,
}

fn masked_fill(on_false: &Tensor, mask: &Tensor, on_true: &Tensor) -> Result<Tensor> {
    let shape = mask.shape();
    let m = mask.where_cond(&on_true.broadcast_as(shape.dims())?, on_false)?;
    Ok(m)
}

impl LayerWeights {
    fn apply_rotary_emb(&self, x: &Tensor, index_pos: usize) -> Result<Tensor> {
        let _enter = self.span_rot.enter();
        let (b_sz, n_head, seq_len, n_embd) = x.dims4()?;
        let cos = self
            .cos
            .narrow(0, index_pos, seq_len)?
            .reshape((seq_len, n_embd / 2, 1))?;
        let sin = self
            .sin
            .narrow(0, index_pos, seq_len)?
            .reshape((seq_len, n_embd / 2, 1))?;
        let cos = cos.broadcast_as((b_sz, 1, seq_len, n_embd / 2, 1))?;
        let sin = sin.broadcast_as((b_sz, 1, seq_len, n_embd / 2, 1))?;
        // This mimics the llama.cpp behavior.
        // https://github.com/ggerganov/llama.cpp/blob/1f0bccb27929e261744c979bc75114955da49e98/ggml.c#L12104-L12105
        // The x0 and x1 value are interleaved on the n_embd (= head_dim) dimension.
        // The resulting y0 and y1 are also interleaved with:
        //   y0 = x0*cos - x1*sin
        //   y1 = x0*sin + x1*cos
        let x = x.reshape((b_sz, n_head, seq_len, n_embd / 2, 2))?;
        let x0 = x.narrow(D::Minus1, 0, 1)?;
        let x1 = x.narrow(D::Minus1, 1, 1)?;
        let y0 = (x0.broadcast_mul(&cos)? - x1.broadcast_mul(&sin)?)?;
        let y1 = (x0.broadcast_mul(&sin)? + x1.broadcast_mul(&cos)?)?;
        let rope = Tensor::cat(&[y0, y1], D::Minus1)?;
        let rope = rope.flatten_from(D::Minus2)?;
        Ok(rope)
    }

    fn forward_attn(&mut self, x: &Tensor, mask: &Tensor, index_pos: usize) -> Result<Tensor> {
        let _enter = self.span_attn.enter();
        let (b_sz, seq_len, n_embd) = x.dims3()?;
        let q = self.attention_wq.forward(x)?;
        let k = self.attention_wk.forward(x)?;
        let v = self.attention_wv.forward(x)?;

        let q = q
            .reshape((b_sz, seq_len, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b_sz, seq_len, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;
        let v = v
            .reshape((b_sz, seq_len, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;

        let q = self.apply_rotary_emb(&q, index_pos)?;
        let k = self.apply_rotary_emb(&k, index_pos)?;

        let (k, v) = match &self.kv_cache {
            None => (k, v),
            Some((k_cache, v_cache)) => {
                if index_pos == 0 {
                    (k, v)
                } else {
                    let k = Tensor::cat(&[k_cache, &k], 2)?.contiguous()?;
                    let v = Tensor::cat(&[v_cache, &v], 2)?.contiguous()?;
                    (k, v)
                }
            }
        };
        self.kv_cache = Some((k.clone(), v.clone()));

        // Support for MQA, useful for 70B models.
        let k = self.repeat_kv(k)?;
        let v = self.repeat_kv(v)?;

        let att = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?;
        let mask = mask.broadcast_as(att.shape())?;
        let att = masked_fill(&att, &mask, &self.neg_inf)?;
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        // Convert to contiguous as matmul doesn't support strided vs for now.
        let y = att.matmul(&v.contiguous()?)?;
        let y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, n_embd])?;
        let y = self.attention_wo.forward(&y)?;
        Ok(y)
    }

    fn repeat_kv(&self, x: Tensor) -> Result<Tensor> {
        let n_rep = self.n_head / self.n_kv_head;
        if n_rep == 1 {
            Ok(x)
        } else {
            let (b_sz, n_kv_head, seq_len, head_dim) = x.dims4()?;
            let x = x
                .unsqueeze(2)?
                .expand((b_sz, n_kv_head, n_rep, seq_len, head_dim))?
                .reshape((b_sz, n_kv_head * n_rep, seq_len, head_dim))?;
            Ok(x)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelWeights {
    tok_embeddings: Embedding,
    layers: Vec<LayerWeights>,
    norm: RmsNorm,
    output: QMatMul,
    masks: HashMap<usize, Tensor>,
    span: tracing::Span,
    span_output: tracing::Span,
}

fn precomput_freqs_cis(
    head_dim: usize,
    freq_base: f32,
    device: &Device,
) -> Result<(Tensor, Tensor)> {
    let theta: Vec<_> = (0..head_dim)
        .step_by(2)
        .map(|i| 1f32 / freq_base.powf(i as f32 / head_dim as f32))
        .collect();
    let theta = Tensor::new(theta.as_slice(), device)?;
    let idx_theta = Tensor::arange(0, MAX_SEQ_LEN as u32, device)?
        .to_dtype(DType::F32)?
        .reshape((MAX_SEQ_LEN, 1))?
        .matmul(&theta.reshape((1, theta.elem_count()))?)?;
    let cos = idx_theta.cos()?;
    let sin = idx_theta.sin()?;
    Ok((cos, sin))
}

impl ModelWeights {
    pub fn from_ggml(mut ct: ggml_file::Content, gqa: usize) -> Result<Self> {
        let head_dim = (ct.hparams.n_embd / ct.hparams.n_head) as usize;
        let (cos, sin) = precomput_freqs_cis(head_dim, 10000., &ct.device)?;
        let neg_inf = Tensor::new(f32::NEG_INFINITY, &ct.device)?;
        let tok_embeddings = ct.remove("tok_embeddings.weight")?;
        let tok_embeddings = tok_embeddings.dequantize(&ct.device)?;
        let norm = RmsNorm::from_qtensor(ct.remove("norm.weight")?, 1e-5)?;
        let output = ct.remove("output.weight")?;
        let mut layers = Vec::with_capacity(ct.hparams.n_layer as usize);
        for layer_idx in 0..ct.hparams.n_layer {
            let prefix = format!("layers.{layer_idx}");
            let attention_wq = ct.remove(&format!("{prefix}.attention.wq.weight"))?;
            let attention_wk = ct.remove(&format!("{prefix}.attention.wk.weight"))?;
            let attention_wv = ct.remove(&format!("{prefix}.attention.wv.weight"))?;
            let attention_wo = ct.remove(&format!("{prefix}.attention.wo.weight"))?;
            let mlp_or_moe = {
                let feed_forward_w1 = ct.remove(&format!("{prefix}.feed_forward.w1.weight"))?;
                let feed_forward_w2 = ct.remove(&format!("{prefix}.feed_forward.w2.weight"))?;
                let feed_forward_w3 = ct.remove(&format!("{prefix}.feed_forward.w3.weight"))?;
                MlpOrMoe::Mlp(Mlp {
                    feed_forward_w1: QMatMul::from_qtensor(feed_forward_w1)?,
                    feed_forward_w2: QMatMul::from_qtensor(feed_forward_w2)?,
                    feed_forward_w3: QMatMul::from_qtensor(feed_forward_w3)?,
                })
            };
            let attention_norm = ct.remove(&format!("{prefix}.attention_norm.weight"))?;
            let ffn_norm = ct.remove(&format!("{prefix}.ffn_norm.weight"))?;
            let span_attn = tracing::span!(tracing::Level::TRACE, "attn");
            let span_rot = tracing::span!(tracing::Level::TRACE, "attn-rot");
            let span_mlp = tracing::span!(tracing::Level::TRACE, "attn-mlp");
            layers.push(LayerWeights {
                attention_wq: QMatMul::from_qtensor(attention_wq)?,
                attention_wk: QMatMul::from_qtensor(attention_wk)?,
                attention_wv: QMatMul::from_qtensor(attention_wv)?,
                attention_wo: QMatMul::from_qtensor(attention_wo)?,
                attention_norm: RmsNorm::from_qtensor(attention_norm, 1e-5)?,
                mlp_or_moe,
                ffn_norm: RmsNorm::from_qtensor(ffn_norm, 1e-5)?,
                n_head: ct.hparams.n_head as usize,
                n_kv_head: ct.hparams.n_head as usize / gqa,
                head_dim: (ct.hparams.n_embd / ct.hparams.n_head) as usize,
                cos: cos.clone(),
                sin: sin.clone(),
                neg_inf: neg_inf.clone(),
                kv_cache: None,
                span_attn,
                span_rot,
                span_mlp,
            })
        }
        let span = tracing::span!(tracing::Level::TRACE, "model");
        let span_output = tracing::span!(tracing::Level::TRACE, "output");
        Ok(Self {
            tok_embeddings: Embedding::new(tok_embeddings, ct.hparams.n_embd as usize),
            layers,
            norm,
            output: QMatMul::from_qtensor(output)?,
            masks: HashMap::new(),
            span,
            span_output,
        })
    }

    pub fn from_gguf<R: std::io::Seek + std::io::Read>(
        ct: gguf_file::Content,
        reader: &mut R,
        device: &Device,
    ) -> Result<Self> {
        let md_get = |s: &str| match ct.metadata.get(s) {
            None => candle::bail!("cannot find {s} in metadata"),
            Some(v) => Ok(v),
        };

        // Parameter extraction from metadata.
        let n_expert = md_get("llama.expert_count")
            .and_then(|v| v.to_u32())
            .unwrap_or(0) as usize;
        let n_expert_used = md_get("llama.expert_used_count")
            .and_then(|v| v.to_u32())
            .unwrap_or(0) as usize;
        let head_count = md_get("llama.attention.head_count")?.to_u32()? as usize;
        let head_count_kv = md_get("llama.attention.head_count_kv")?.to_u32()? as usize;
        let block_count = md_get("llama.block_count")?.to_u32()? as usize;
        let embedding_length = md_get("llama.embedding_length")?.to_u32()? as usize;
        let rope_dim = md_get("llama.rope.dimension_count")?.to_u32()? as usize;
        // Strangely this value is generally 1e-6 in GGUF file but used to be 1e-5 by default.
        let rms_norm_eps = md_get("llama.attention.layer_norm_rms_epsilon")?.to_f64()?;

        let rope_freq_base = md_get("llama.rope.freq_base")
            .and_then(|m| m.to_f32())
            .unwrap_or(10000f32);
        let (cos, sin) = precomput_freqs_cis(rope_dim, rope_freq_base, device)?;
        let neg_inf = Tensor::new(f32::NEG_INFINITY, device)?;

        let tok_embeddings = ct.tensor(reader, "token_embd.weight", device)?;
        let tok_embeddings = tok_embeddings.dequantize(device)?;
        let norm = RmsNorm::from_qtensor(
            ct.tensor(reader, "output_norm.weight", device)?,
            rms_norm_eps,
        )?;
        let output = ct.tensor(reader, "output.weight", device)?;
        let mut layers = Vec::with_capacity(block_count);
        for layer_idx in 0..block_count {
            let prefix = format!("blk.{layer_idx}");
            let attention_wq = ct.tensor(reader, &format!("{prefix}.attn_q.weight"), device)?;
            let attention_wk = ct.tensor(reader, &format!("{prefix}.attn_k.weight"), device)?;
            let attention_wv = ct.tensor(reader, &format!("{prefix}.attn_v.weight"), device)?;
            let attention_wo =
                ct.tensor(reader, &format!("{prefix}.attn_output.weight"), device)?;
            let mlp_or_moe = if n_expert <= 1 {
                let feed_forward_w1 =
                    ct.tensor(reader, &format!("{prefix}.ffn_gate.weight"), device)?;
                let feed_forward_w2 =
                    ct.tensor(reader, &format!("{prefix}.ffn_down.weight"), device)?;
                let feed_forward_w3 =
                    ct.tensor(reader, &format!("{prefix}.ffn_up.weight"), device)?;
                MlpOrMoe::Mlp(Mlp {
                    feed_forward_w1: QMatMul::from_qtensor(feed_forward_w1)?,
                    feed_forward_w2: QMatMul::from_qtensor(feed_forward_w2)?,
                    feed_forward_w3: QMatMul::from_qtensor(feed_forward_w3)?,
                })
            } else {
                let feed_forward_gate_inp =
                    ct.tensor(reader, &format!("{prefix}.ffn_gate_inp.weight"), device)?;
                let mut experts = Vec::with_capacity(n_expert);
                for i in 0..n_expert {
                    let feed_forward_w1 =
                        ct.tensor(reader, &format!("{prefix}.ffn_gate.{i}.weight"), device)?;
                    let feed_forward_w2 =
                        ct.tensor(reader, &format!("{prefix}.ffn_down.{i}.weight"), device)?;
                    let feed_forward_w3 =
                        ct.tensor(reader, &format!("{prefix}.ffn_up.{i}.weight"), device)?;
                    experts.push(Mlp {
                        feed_forward_w1: QMatMul::from_qtensor(feed_forward_w1)?,
                        feed_forward_w2: QMatMul::from_qtensor(feed_forward_w2)?,
                        feed_forward_w3: QMatMul::from_qtensor(feed_forward_w3)?,
                    })
                }
                MlpOrMoe::MoE {
                    n_expert_used,
                    feed_forward_gate_inp: QMatMul::from_qtensor(feed_forward_gate_inp)?,
                    experts,
                }
            };
            let attention_norm =
                ct.tensor(reader, &format!("{prefix}.attn_norm.weight"), device)?;
            let ffn_norm = ct.tensor(reader, &format!("{prefix}.ffn_norm.weight"), device)?;
            let span_attn = tracing::span!(tracing::Level::TRACE, "attn");
            let span_rot = tracing::span!(tracing::Level::TRACE, "attn-rot");
            let span_mlp = tracing::span!(tracing::Level::TRACE, "attn-mlp");
            layers.push(LayerWeights {
                attention_wq: QMatMul::from_qtensor(attention_wq)?,
                attention_wk: QMatMul::from_qtensor(attention_wk)?,
                attention_wv: QMatMul::from_qtensor(attention_wv)?,
                attention_wo: QMatMul::from_qtensor(attention_wo)?,
                attention_norm: RmsNorm::from_qtensor(attention_norm, rms_norm_eps)?,
                mlp_or_moe,
                ffn_norm: RmsNorm::from_qtensor(ffn_norm, rms_norm_eps)?,
                n_head: head_count,
                n_kv_head: head_count_kv,
                head_dim: embedding_length / head_count,
                cos: cos.clone(),
                sin: sin.clone(),
                neg_inf: neg_inf.clone(),
                kv_cache: None,
                span_attn,
                span_rot,
                span_mlp,
            })
        }
        let span = tracing::span!(tracing::Level::TRACE, "model");
        let span_output = tracing::span!(tracing::Level::TRACE, "output");
        Ok(Self {
            tok_embeddings: Embedding::new(tok_embeddings, embedding_length),
            layers,
            norm,
            output: QMatMul::from_qtensor(output)?,
            masks: HashMap::new(),
            span,
            span_output,
        })
    }

    fn mask(&mut self, t: usize, device: &Device) -> Result<Tensor> {
        if let Some(mask) = self.masks.get(&t) {
            Ok(mask.clone())
        } else {
            let mask: Vec<_> = (0..t)
                .flat_map(|i| (0..t).map(move |j| u8::from(j > i)))
                .collect();
            let mask = Tensor::from_slice(&mask, (t, t), device)?;
            self.masks.insert(t, mask.clone());
            Ok(mask)
        }
    }

    pub fn forward(&mut self, x: &Tensor, index_pos: usize) -> Result<Tensor> {
        let (_b_sz, seq_len) = x.dims2()?;
        let mask = self.mask(seq_len, x.device())?;
        let _enter = self.span.enter();
        let mut layer_in = self.tok_embeddings.forward(x)?;
        for layer in self.layers.iter_mut() {
            let x = layer_in;
            let residual = &x;
            let x = layer.attention_norm.forward(&x)?;
            let attn = layer.forward_attn(&x, &mask, index_pos)?;
            let x = (attn + residual)?;

            // MLP
            let _enter = layer.span_mlp.enter();
            let residual = &x;
            let x = layer.ffn_norm.forward(&x)?;
            let x = layer.mlp_or_moe.forward(&x)?;
            let x = (x + residual)?;
            layer_in = x
        }
        let x = self.norm.forward(&layer_in)?;
        let x = x.i((.., seq_len - 1, ..))?.contiguous()?;
        let _enter = self.span_output.enter();
        self.output.forward(&x)
    }
}
