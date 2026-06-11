use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::runtime::contracts::{RuntimeError, RuntimeResult};

use super::{
    ContextEmbeddingProviderKind, EmbeddingInput, EmbeddingProvider, EmbeddingVector,
};

/// Token cap per embedded input; bounds inference latency on dense code.
const LOCAL_ONNX_MAX_TOKENS: usize = 2048;
/// Inputs per inference batch.
const LOCAL_ONNX_BATCH: usize = 8;

/// Local transformer embeddings via ONNX Runtime (R8 evaluation tier):
/// a code-tuned embedding model (e.g. `jina-embeddings-v2-base-code`)
/// loaded from a directory holding `model.onnx` (or
/// `model_quantized.onnx`) and `tokenizer.json`. Mean-pools the last
/// hidden state under the attention mask and L2-normalizes, the standard
/// sentence-embedding readout. Runs entirely on-host: restricted
/// evidence may be embedded, same as the hash provider.
pub struct LocalOnnxEmbeddingProvider {
    session: Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
    needs_token_type_ids: bool,
}

impl LocalOnnxEmbeddingProvider {
    /// Process-wide provider per model directory: the session (model
    /// weights) loads once and is reused across index builds and query
    /// embeddings.
    pub fn shared(model_dir: &Path) -> RuntimeResult<std::sync::Arc<Self>> {
        static REGISTRY: std::sync::OnceLock<
            Mutex<std::collections::HashMap<std::path::PathBuf, std::sync::Arc<LocalOnnxEmbeddingProvider>>>,
        > = std::sync::OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut providers = registry.lock().expect("ONNX provider registry poisoned");
        if let Some(provider) = providers.get(model_dir) {
            return Ok(std::sync::Arc::clone(provider));
        }
        let provider = std::sync::Arc::new(Self::from_model_dir(model_dir)?);
        providers.insert(model_dir.to_path_buf(), std::sync::Arc::clone(&provider));
        Ok(provider)
    }

    pub fn from_model_dir(model_dir: &Path) -> RuntimeResult<Self> {
        let model_path = ["model.onnx", "model_quantized.onnx"]
            .iter()
            .map(|name| model_dir.join(name))
            .find(|path| path.exists())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "no model.onnx or model_quantized.onnx under {}",
                    model_dir.display()
                ))
            })?;
        let session = ort::session::Session::builder()
            .and_then(|mut builder| builder.commit_from_file(&model_path))
            .map_err(|error| {
                RuntimeError::ProviderMessage {
                    status: None,
                    retryable: false,
                    message: format!("failed to load ONNX embedding model: {error}"),
                }
            })?;
        let needs_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");
        let mut tokenizer = tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|error| RuntimeError::ProviderMessage {
                status: None,
                retryable: false,
                message: format!("failed to load embedding tokenizer: {error}"),
            })?;
        tokenizer.with_truncation(Some(tokenizers::TruncationParams {
            max_length: LOCAL_ONNX_MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(|error| RuntimeError::ProviderMessage {
            status: None,
            retryable: false,
            message: format!("failed to configure tokenizer truncation: {error}"),
        })?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            needs_token_type_ids,
        })
    }

    fn embed_batch(&self, texts: &[&str]) -> RuntimeResult<Vec<EmbeddingVector>> {
        let provider_error = |message: String| RuntimeError::ProviderMessage {
            status: None,
            retryable: false,
            message,
        };
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|error| provider_error(format!("embedding tokenization failed: {error}")))?;
        let batch = encodings.len();
        let width = encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .max()
            .unwrap_or(1)
            .max(1);
        let mut input_ids = vec![0i64; batch * width];
        let mut attention_mask = vec![0i64; batch * width];
        for (row, encoding) in encodings.iter().enumerate() {
            for (column, (&id, &mask)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .enumerate()
            {
                input_ids[row * width + column] = i64::from(id);
                attention_mask[row * width + column] = i64::from(mask);
            }
        }
        let shape = [batch as i64, width as i64];
        let ids_tensor = ort::value::Tensor::from_array((shape, input_ids))
            .map_err(|error| provider_error(format!("embedding input tensor failed: {error}")))?;
        let mask_tensor = ort::value::Tensor::from_array((shape, attention_mask.clone()))
            .map_err(|error| provider_error(format!("embedding mask tensor failed: {error}")))?;
        let mut inputs: Vec<(std::borrow::Cow<'_, str>, ort::session::SessionInputValue<'_>)> = vec![
            ("input_ids".into(), ids_tensor.into()),
            ("attention_mask".into(), mask_tensor.into()),
        ];
        if self.needs_token_type_ids {
            let type_tensor = ort::value::Tensor::from_array((shape, vec![0i64; batch * width]))
                .map_err(|error| {
                    provider_error(format!("embedding type tensor failed: {error}"))
                })?;
            inputs.push(("token_type_ids".into(), type_tensor.into()));
        }
        let mut session = self.session.lock().expect("ONNX session poisoned");
        let outputs = session
            .run(inputs)
            .map_err(|error| provider_error(format!("ONNX embedding inference failed: {error}")))?;
        let (output_shape, hidden) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|error| provider_error(format!("ONNX embedding output failed: {error}")))?;
        let dims = output_shape
            .last()
            .copied()
            .ok_or_else(|| provider_error("ONNX output has no dimensions".to_string()))?
            as usize;
        // Mean pool the last hidden state under the attention mask.
        let mut vectors = Vec::with_capacity(batch);
        for row in 0..batch {
            let mut pooled = vec![0.0f32; dims];
            let mut token_count = 0.0f32;
            for column in 0..width {
                if attention_mask[row * width + column] == 0 {
                    continue;
                }
                token_count += 1.0;
                let offset = (row * width + column) * dims;
                for (value, hidden_value) in pooled.iter_mut().zip(&hidden[offset..offset + dims])
                {
                    *value += hidden_value;
                }
            }
            if token_count > 0.0 {
                for value in &mut pooled {
                    *value /= token_count;
                }
            }
            vectors.push(EmbeddingVector::normalized(pooled));
        }
        Ok(vectors)
    }
}

#[async_trait]
impl EmbeddingProvider for LocalOnnxEmbeddingProvider {
    fn kind(&self) -> ContextEmbeddingProviderKind {
        ContextEmbeddingProviderKind::LocalOnnx
    }

    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>> {
        let mut vectors = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(LOCAL_ONNX_BATCH) {
            let texts = batch.iter().map(|input| input.text.as_str()).collect::<Vec<_>>();
            vectors.extend(self.embed_batch(&texts)?);
        }
        Ok(vectors)
    }
}
