use std::sync::{Arc, Mutex};

use candle_core::{DType, Device, Result, Tensor};
use rand_isaac::Isaac64Rng;

use crate::{
    aici::toktree::TokTrie,
    get_bias_if_not_allowed, sample_async,
    sampler::Logprobs,
    sequence::{Sequence, SequenceRecognizer},
};

pub async fn sample_sequence(
    logits: Tensor,
    seq: &mut Sequence,
    return_logprobs: bool,
    repeat_last_n: usize,
    tok_trie: Arc<TokTrie>,
    rng: Arc<Mutex<Isaac64Rng>>,
    use_async_pool: bool,
) -> Result<Logprobs> {
    let logits = logits.squeeze(0)?.squeeze(0)?.to_dtype(DType::F32)?;
    let start_at = seq.get_toks().len().saturating_sub(repeat_last_n);

    let sampler = seq.sampler();
    let logits_clone = logits.clone();
    let ctx_clone = seq.get_toks()[start_at..].to_vec();
    let rng_clone = rng.clone();
    let first_lobprobs_response = sample_async!(
        use_async_pool,
        sampler,
        logits_clone,
        ctx_clone,
        return_logprobs,
        rng_clone
    );

    let bias_if_not_allowed = match &mut seq.recognizer {
        SequenceRecognizer::Regex(ref mut rx) => {
            get_bias_if_not_allowed!(tok_trie, rx.as_mut(), first_lobprobs_response.token)
        }
        SequenceRecognizer::Cfg(ref mut cfg) => {
            get_bias_if_not_allowed!(tok_trie, cfg.as_mut(), first_lobprobs_response.token)
        }
        SequenceRecognizer::None => None,
    };
    let second_logprobs_response = match bias_if_not_allowed {
        Some(token_set) => {
            let mut acc = vec![-f32::INFINITY; tok_trie.vocab_size()];
            token_set.apply_to(&mut acc);
            let new_logits = (logits + Tensor::from_slice(&acc, acc.len(), &Device::Cpu)?)?;

            let ctx_clone = seq.get_toks()[start_at..].to_vec();
            let rng_clone = rng.clone();
            let sampler = seq.sampler();
            sample_async!(
                use_async_pool,
                sampler,
                new_logits,
                ctx_clone,
                return_logprobs,
                rng_clone
            )
        }
        None => first_lobprobs_response,
    };

    match seq.recognizer {
        SequenceRecognizer::Regex(ref mut rx) => {
            tok_trie.append_token(rx.as_mut(), second_logprobs_response.token);
        }
        SequenceRecognizer::Cfg(ref mut cfg) => {
            tok_trie.append_token(cfg.as_mut(), second_logprobs_response.token);
        }
        SequenceRecognizer::None => {}
    }
    Ok(second_logprobs_response)
}

pub async fn sample_target_sequence_speculative(
    logits: Tensor,
    seq: &mut Sequence,
    return_logprobs: bool,
    repeat_last_n: usize,
    tok_trie: Arc<TokTrie>,
    rng: Arc<Mutex<Isaac64Rng>>,
    n_toks: usize,
) -> Result<Vec<Logprobs>> {
    let mut sampled = Vec::new();
    for chunk in logits.chunk(n_toks, 1)? {
        sampled.push(
            sample_sequence(
                chunk,
                seq,
                return_logprobs,
                repeat_last_n,
                tok_trie.clone(),
                rng.clone(),
                true, // TODO: does this hurt perf?
            )
            .await?,
        );
    }
    Ok(sampled)
}
