use super::{
    calculate_inputs, get_model_paths, get_xlora_paths, ChatTemplate, Loader, ModelInputs,
    ModelKind, ModelPaths, Pipeline, TokenSource, XLoraPaths,
};
use crate::aici::bintokens::build_tok_trie;
use crate::aici::toktree::TokTrie;
use crate::models::llama::MAX_SEQ_LEN;
use crate::models::Cache;
use crate::pipeline::calculate_eos_tok;
use crate::xlora_models::{NonGranularState, XLoraConfig, XLoraLlama};
use crate::{deserialize_chat_template, get_paths};
use crate::{
    models::llama::{Llama as NormalModel, LlamaConfig},
    sequence::Sequence,
    utils::{tokens::get_token, varbuilder_utils::from_mmaped_safetensors},
};
use anyhow::Result;
use candle_core::{DType, Device, Tensor};
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use mistralrs_lora::{LoraConfig, Ordering};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokenizers::Tokenizer;
use tracing::info;

enum Model {
    Normal(NormalModel),
    XLoraNormal(XLoraLlama),
}
pub const LLAMA_IS_GPTX: bool = true;

pub struct LlamaModelPaths<P> {
    tokenizer_filename: P,
    config_filename: P,
    template_filename: P,
    filenames: Vec<P>,
    xlora_adapter_filenames: Option<Vec<(String, P)>>,
    xlora_adapter_configs: Option<Vec<(String, LoraConfig)>>,
    classifier_path: Option<P>,
    classifier_config: Option<XLoraConfig>,
    xlora_ordering: Option<Ordering>,
}

impl ModelPaths for LlamaModelPaths<PathBuf> {
    fn get_config_filename(&self) -> &PathBuf {
        &self.config_filename
    }
    fn get_tokenizer_filename(&self) -> &PathBuf {
        &self.tokenizer_filename
    }
    fn get_weight_filenames(&self) -> &[PathBuf] {
        &self.filenames
    }
    fn get_adapter_filenames(&self) -> &Option<Vec<(String, PathBuf)>> {
        &self.xlora_adapter_filenames
    }
    fn get_adapter_configs(&self) -> &Option<Vec<(String, LoraConfig)>> {
        &self.xlora_adapter_configs
    }
    fn get_classifier_config(&self) -> &Option<XLoraConfig> {
        &self.classifier_config
    }
    fn get_classifier_path(&self) -> &Option<PathBuf> {
        &self.classifier_path
    }
    fn get_ordering(&self) -> &Option<Ordering> {
        &self.xlora_ordering
    }
    fn get_template_filename(&self) -> &PathBuf {
        &self.template_filename
    }
}

pub struct LlamaPipeline {
    model: Model,
    tokenizer: Arc<Tokenizer>,
    tok_trie: TokTrie,
    config: LlamaSpecificConfig,
    no_kv_cache: bool,
    chat_template: ChatTemplate,
    non_granular_state: Option<NonGranularState>,
    model_id: String,
    is_lora: bool,
    eos_tok: Vec<u32>,
}

pub struct LlamaLoader {
    model_id: String,
    config: LlamaSpecificConfig,
    quantized_model_id: Option<String>,
    quantized_filename: Option<String>,
    xlora_model_id: Option<String>,
    kind: ModelKind,
    xlora_order: Option<Ordering>,
    no_kv_cache: bool,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,
    tgt_non_granular_index: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct LlamaSpecificConfig {
    pub repeat_last_n: usize,
    pub use_flash_attn: bool,
    pub gqa: usize,
}

#[derive(Error, Debug)]
enum TokenizerError {
    #[error("`{0}`")]
    Error(String),
}

impl LlamaLoader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: Option<String>,
        config: LlamaSpecificConfig,
        quantized_model_id: Option<String>,
        quantized_filename: Option<String>,
        xlora_model_id: Option<String>,
        kind: ModelKind,
        xlora_order: Option<Ordering>,
        no_kv_cache: bool,
        chat_template: Option<String>,
        tokenizer_json: Option<String>,
        tgt_non_granular_index: Option<usize>,
    ) -> Self {
        let model_id = if let Some(id) = model_id {
            id
        } else {
            info!(
                "Using adapter base model ID: `{}`",
                xlora_order.as_ref().unwrap().base_model_id
            );
            xlora_order.as_ref().unwrap().base_model_id.clone()
        };

        Self {
            model_id,
            config,
            quantized_model_id,
            quantized_filename,
            xlora_model_id,
            kind,
            xlora_order,
            no_kv_cache,
            chat_template,
            tokenizer_json,
            tgt_non_granular_index,
        }
    }
}

impl Loader for LlamaLoader {
    fn download_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
    ) -> Result<Box<dyn ModelPaths>> {
        get_paths!(LlamaModelPaths, &token_source, revision, self)
    }

    fn _setup_model(
        &self,
        paths: &dyn ModelPaths,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<Box<Mutex<dyn Pipeline + Send + Sync>>> {
        let basic_config: LlamaConfig =
            serde_json::from_slice(&std::fs::read(paths.get_config_filename())?)?;
        let default_dtype = if device.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };

        info!("Model config: {basic_config:?}");

        let mut is_lora = false;
        let model = match self.kind {
            ModelKind::QuantizedGGUF => unreachable!(),
            ModelKind::QuantizedGGML => todo!(),
            ModelKind::Normal => {
                let vb = from_mmaped_safetensors(
                    paths.get_weight_filenames().to_vec(),
                    Vec::new(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = NormalModel::load(
                    vb,
                    &basic_config.into_config(self.config.use_flash_attn),
                    device,
                )?;
                Model::Normal(model)
            }
            ModelKind::XLoraNormal => {
                let mut safetensors_paths = paths.get_weight_filenames().iter().collect::<Vec<_>>();
                safetensors_paths.push(paths.get_classifier_path().as_ref().unwrap());
                let vb = from_mmaped_safetensors(
                    safetensors_paths
                        .iter()
                        .map(|x| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    paths
                        .get_adapter_filenames()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|(_, x)| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = XLoraLlama::load(
                    vb,
                    &basic_config.into_config(self.config.use_flash_attn),
                    dtype.unwrap_or(default_dtype),
                    device,
                    paths.get_adapter_configs().as_ref().unwrap(),
                    Some(paths.get_classifier_config().as_ref().unwrap().clone()),
                    paths.get_ordering().as_ref().unwrap().clone(),
                )?;
                Model::XLoraNormal(model)
            }
            ModelKind::XLoraGGUF => unreachable!(),
            ModelKind::XLoraGGML => todo!(),
            ModelKind::LoraGGUF => unreachable!(),
            ModelKind::LoraGGML => unreachable!(),
            ModelKind::LoraNormal => {
                let vb = from_mmaped_safetensors(
                    paths.get_weight_filenames().to_vec(),
                    paths
                        .get_adapter_filenames()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|(_, x)| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = XLoraLlama::load(
                    vb,
                    &basic_config.into_config(self.config.use_flash_attn),
                    dtype.unwrap_or(default_dtype),
                    device,
                    paths.get_adapter_configs().as_ref().unwrap(),
                    None,
                    paths.get_ordering().as_ref().unwrap().clone(),
                )?;
                is_lora = true;
                Model::XLoraNormal(model)
            }
        };

        let tokenizer = Tokenizer::from_file(paths.get_tokenizer_filename())
            .map_err(|e| TokenizerError::Error(e.to_string()))?;

        let chat_template: ChatTemplate = deserialize_chat_template!(paths, self);

        let mut eos_toks = vec![chat_template.eos_tok()];

        // Handle Llama3 chat case
        if tokenizer.encode("<|eot_id|>", true).is_ok() {
            eos_toks.push("<|eot_id|>".to_string())
        }

        info!(
            "bos_tok = {}, eos_tok = {:?}, unk_tok = {}",
            chat_template.bos_tok(),
            eos_toks,
            chat_template.eos_tok()
        );

        Ok(Box::new(Mutex::new(LlamaPipeline {
            model,
            eos_tok: calculate_eos_tok(eos_toks, &tokenizer),
            tok_trie: build_tok_trie(tokenizer.clone()),
            tokenizer: tokenizer.into(),
            config: self.config,
            no_kv_cache: self.no_kv_cache,
            chat_template,
            non_granular_state: self.tgt_non_granular_index.map(|tgt_non_granular_index| {
                NonGranularState {
                    non_granular_index: Arc::new(Mutex::new(0)),
                    tgt_non_granular_index,
                }
            }),
            model_id: self.model_id.clone(),
            is_lora,
        })))
    }

    fn get_id(&self) -> &str {
        self.xlora_model_id.as_deref().unwrap_or(&self.model_id)
    }

    fn get_kind(&self) -> ModelKind {
        self.kind
    }
}

impl Pipeline for LlamaPipeline {
    fn forward(
        &mut self,
        input_toks: &[&mut Sequence],
        is_prompt: bool,
    ) -> Result<Tensor, candle_core::Error> {
        let ModelInputs {
            input_ids,
            input_ids_full,
            seqlen_offsets,
            seqlen_offsets_full,
            seqlen_offsets_kernel,
            seqlen_offsets_kernel_full,
            context_lens,
        } = calculate_inputs(
            input_toks,
            is_prompt,
            self.is_xlora(),
            self.device(),
            self.no_kv_cache,
        )
        .unwrap();
        match self.model {
            Model::Normal(ref mut model) => model.forward(
                &input_ids,
                &seqlen_offsets,
                seqlen_offsets_kernel,
                context_lens,
            ),
            Model::XLoraNormal(ref mut model) => model.forward(
                &input_ids,
                input_ids_full.as_ref().unwrap_or(&input_ids),
                &seqlen_offsets,
                seqlen_offsets_full.as_ref().unwrap_or(&seqlen_offsets),
                seqlen_offsets_kernel.clone(),
                seqlen_offsets_kernel_full.unwrap_or(seqlen_offsets_kernel),
                self.no_kv_cache,
                &self.non_granular_state,
                context_lens,
            ),
        }
    }
    fn device(&self) -> &Device {
        match self.model {
            Model::Normal(ref model) => &model.device,
            Model::XLoraNormal(ref model) => &model.device,
        }
    }
    fn num_hidden_layers(&self) -> usize {
        self.cache().lock().len()
    }
    fn cache(&self) -> &Cache {
        match self.model {
            Model::Normal(ref model) => &model.kv_cache,
            Model::XLoraNormal(ref model) => &model.kv_cache,
        }
    }
    fn get_repeat_last_n(&self) -> usize {
        self.config.repeat_last_n
    }
    fn tokenizer(&self) -> Arc<Tokenizer> {
        self.tokenizer.clone()
    }
    fn eos_tok(&self) -> &[u32] {
        &self.eos_tok
    }
    fn name(&self) -> String {
        self.model_id.clone()
    }
    fn get_max_seq_len(&self) -> usize {
        match &self.model {
            Model::Normal(_) | Model::XLoraNormal(_) => MAX_SEQ_LEN,
        }
    }
    fn is_xlora(&self) -> bool {
        match &self.model {
            Model::Normal(_) => false,
            Model::XLoraNormal(_) => !self.is_lora,
        }
    }
    fn has_no_kv_cache(&self) -> bool {
        self.no_kv_cache
    }
    fn get_chat_template(&self) -> &ChatTemplate {
        &self.chat_template
    }
    fn get_non_granular_state(&self) -> &Option<NonGranularState> {
        &self.non_granular_state
    }

    fn tok_trie(&self) -> &TokTrie {
        &self.tok_trie
    }
}
