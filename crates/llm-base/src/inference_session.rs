use ggml::{Buffer, ComputationGraph, Context, GraphExecutionPlan, Tensor};
use serde::Serialize;
use std::{cell::RefCell, fmt::Display, sync::Arc};
use thiserror::Error;
use tracing::{instrument, log};

#[cfg(feature = "metal")]
use ggml::accelerator::metal::MetalContext;

use crate::{
    mulf, util, InferenceParameters, Model, ModelParameters, OutputRequest, Prompt, TokenId,
    TokenUtf8Buffer, TokenizationError,
};

// The size of a scratch buffer used for inference. This is used for temporary
// storage of intermediate results during inference.
//
// The specific value was copied from `llama.cpp`.
const SCRATCH_SIZE: usize = 512 * 1024 * 1024;

type ScratchBuffers = [ggml::Buffer; 2];

fn scratch_buffers() -> ScratchBuffers {
    [
        ggml::Buffer::new(SCRATCH_SIZE),
        ggml::Buffer::new(SCRATCH_SIZE),
    ]
}

/// Result of graph building
pub struct GraphOutputs {
    /// The output containing the model's result
    pub result: Tensor,

    /// The output containing embeddings
    pub embedding_result: Tensor,
}

/// An inference session represents the state of the text generation. This holds
/// the full context window, as well as several additional parameters used
/// during sampling.
///
/// # Safety
/// This implements `Send` as it can be sent to another thread. However, it does
/// not implement `Sync` - it *cannot* be used from multiple threads at the same time.
///
/// Consider spawning multiple inference sessions for the same model if you need
/// to use it from multiple threads.
pub struct InferenceSession {
    // Must be kept alive for the model
    _session_ctx: Arc<ggml::Context>,

    // Original size of the memory used to create this context.
    _memory_size: usize,

    // Configuration for the session.
    pub(crate) config: InferenceSessionConfig,

    /// Memory K
    #[doc(hidden)]
    pub memory_k: ggml::Tensor,

    /// Memory M
    #[doc(hidden)]
    pub memory_v: ggml::Tensor,

    /// How many tokens have been fed into the model's working memory so far.
    #[doc(hidden)]
    pub n_past: usize,

    /// How much memory is required per token for the temporary context used
    /// during inference.
    #[doc(hidden)]
    pub mem_per_token: usize,

    /// All tokens generated by this inference session
    pub(crate) tokens: Vec<TokenId>,

    // All decoded tokens generated by this inference session
    pub(crate) decoded_tokens: Vec<u8>,

    /// The logits that were last predicted by the network. Zeroed out otherwise.
    #[doc(hidden)]
    pub last_logits: Vec<f32>,

    #[cfg(feature = "metal")]
    metal_context: Option<MetalContext>,

    ctx0: Context,

    n_embd: usize,

    scratch: ScratchBuffers,
}

pub struct BuildContext<'session> {
    //FIXME: Borrowing issue, dont know how to fix it
    pub ctx0: RefCell<&'session mut Context>,
    pub embd: &'session Tensor,
    pub memory_k: &'session Tensor,
    pub memory_v: &'session Tensor,
    pub scratch: &'session ScratchBuffers,
}

impl<'session> BuildContext<'session> {
    pub fn get_scratch(&self, idx: usize) -> Option<&Buffer> {
        Some(&self.scratch[idx])
    }
}

unsafe impl Send for InferenceSession {}
impl InferenceSession {
    /// Create a new InferenceSession
    pub fn new(
        config: InferenceSessionConfig,
        params: &ModelParameters,
        n_layer: usize,
        n_embd: usize,
        n_vocab: usize,
    ) -> InferenceSession {
        let ModelParameters {
            use_gpu,
            context_size,
            ..
        } = *params;

        let context_byte_size = {
            let mut size = 0;
            size += mulf!(
                context_size,
                n_layer,
                n_embd,
                ggml::type_sizef(config.memory_k_type.into())
            ); // memory_k
            size += mulf!(
                context_size,
                n_layer,
                n_embd,
                ggml::type_sizef(config.memory_v_type.into())
            ); // memory_v
            size += (5 + 10 * n_layer) * 256; // object overhead

            size
        };

        if use_gpu {
            ggml::accelerator::initialize(0);
            ggml::accelerator::set_scratch_size(config.n_batch * 1024 * 1024);
        }

        let session_ctx = Arc::new(ggml::Context::new_with_allocate(context_byte_size));

        // Initialize key + value memory tensors
        let n_mem = n_layer * context_size;
        let n_elements = n_embd * n_mem;
        let (memory_k, memory_v) = kv_memory(&session_ctx, &config, use_gpu, n_elements);

        let scratch = scratch_buffers();

        // Allocate buffer for storing intermediate values during evaluation (ctx0 backing)
        // For the first run, we need to guess a maximum buffer size so we can measure
        // the actual memory consumption of the temporary ggml context.
        //
        // These numbers are from `llama.cpp`, and could potentially be more efficient.
        let buf_size = {
            let buf_size_mb = if n_layer >= 80 {
                1536
            } else if n_layer >= 60 {
                1280
            } else {
                1024
            };
            buf_size_mb * 1024 * 1024
        };

        let eval = Buffer::new(buf_size);
        let ctx0 = ggml::Context::new_with_buffer(eval);

        // Set up Metal support
        #[cfg(feature = "metal")]
        let metal_context = {
            if use_gpu {
                let mut metal_context = MetalContext::new(config.n_threads);
                metal_context.add_scratch_buffer(ctx0.storage().as_buffer().unwrap());

                for buf in scratch.iter() {
                    metal_context.add_scratch_buffer(buf);
                }
                metal_context.add_context(session_ctx.clone());
                Some(metal_context)
            } else {
                None
            }
        };

        InferenceSession {
            _session_ctx: session_ctx,
            _memory_size: context_byte_size,
            config,
            memory_k,
            memory_v,
            n_past: 0,
            mem_per_token: 0,
            tokens: vec![],
            decoded_tokens: vec![],
            last_logits: vec![0.0; n_vocab],
            #[cfg(feature = "metal")]
            metal_context,
            ctx0,
            n_embd,
            scratch,
        }
    }

    /// Compute a model (possibly building a graph in the provided closure when called for the first time and/or when parameters have)
    pub fn compute<F>(
        &mut self,
        #[allow(unused_variables)] model_context: Arc<Context>,
        input_tokens: &[TokenId],
        builder: F,
    ) -> GraphOutputs
    where
        F: FnOnce(BuildContext) -> (ComputationGraph, GraphOutputs),
    {
        // Build a graph
        self.ctx0.recreate();
        let ctx0 = &mut self.ctx0;
        let mut embd = ctx0
            .new_tensor_1d(ggml::Type::I32, input_tokens.len())
            .set_name("embd");

        let bc = BuildContext {
            ctx0: RefCell::new(ctx0),
            embd: &embd,
            memory_k: &self.memory_k,
            memory_v: &self.memory_v,
            scratch: &mut self.scratch,
        };
        let (mut built_gf, built_result) = builder(bc);

        // Do Metal'y stuff
        #[cfg(feature = "metal")]
        {
            if let Some(ref mut metal_context) = self.metal_context {
                metal_context.add_context(model_context);
            }
        }

        // Write input tokens
        unsafe { embd.write_data(bytemuck::cast_slice(input_tokens)) };

        // Compute the graph
        built_gf.build_forward_expand(&built_result.result);

        #[cfg(feature = "metal")]
        {
            // FIXME can only process one token at a time currently
            // See https://github.com/ggerganov/llama.cpp/blob/e1886cf4fe0d0f31661dda52a4a9f34bd9b9009a/llama.cpp#L1692
            if input_tokens.len() == 1 {
                if let Some(ref metal_context) = self.metal_context {
                    metal_context.graph_compute(&mut built_gf);
                    metal_context.get_tensor(&built_result.result);
                } else {
                    let mut plan = GraphExecutionPlan::new(&mut built_gf, self.config.n_threads);
                    plan.execute(ctx0);
                }
            } else {
                let mut plan = GraphExecutionPlan::new(&mut built_gf, self.config.n_threads);
                plan.execute(ctx0);
            }
        }
        #[cfg(not(feature = "metal"))]
        {
            let mut plan = GraphExecutionPlan::new(&mut built_gf, self.config.n_threads);
            plan.execute(ctx0);
        }

        // Adjust the required memory per token if we didn't know that already
        if self.mem_per_token == 0 {
            self.mem_per_token = ctx0.used_mem() / self.n_embd;
        }

        // Adjust n_past to new length.
        self.n_past += input_tokens.len();

        // Safety: ctx0 will linger around
        GraphOutputs {
            result: built_result.result.share(),
            embedding_result: built_result.embedding_result.share(),
        }
    }

    /// Feed a prompt to the model for this session.
    #[instrument(skip_all)]
    pub fn feed_prompt<'a, E: std::error::Error + Send + Sync + 'static, P: Into<Prompt<'a>>>(
        &mut self,
        model: &dyn Model,
        prompt: P,
        output_request: &mut OutputRequest,
        mut callback: impl FnMut(&[u8]) -> Result<InferenceFeedback, E>,
    ) -> Result<(), InferenceError> {
        let beginning_of_sentence = self.n_past == 0;

        let vocab = model.tokenizer();
        let prompt_tokens = prompt.into().to_tokens(vocab, beginning_of_sentence)?;

        if self.n_past + prompt_tokens.len() >= model.context_size() {
            return Err(InferenceError::ContextFull);
        }

        for batch in prompt_tokens.chunks(self.config.n_batch) {
            model.evaluate(self, batch, output_request);
            for &tk in batch {
                let should_call_callback = Some(tk) != model.bot_token_id();

                let mut token = match model.tokenizer() {
                    crate::Tokenizer::Embedded(_) => model.tokenizer().token(tk as usize).to_vec(),
                    crate::Tokenizer::HuggingFace(_) => {
                        let mut tokens = self.tokens.clone();
                        tokens.push(tk);

                        get_newly_decoded_portion_huggingface(model, tokens, &self.decoded_tokens)
                    }
                };

                if should_call_callback {
                    // NOTE: No string ever tokenizes to the end of sentence. So we
                    // can just return the id here.
                    match callback(&token) {
                        Err(e) => return Err(InferenceError::UserCallback(Box::new(e))),
                        Ok(f) => match f {
                            InferenceFeedback::Continue => (),
                            InferenceFeedback::Halt => break,
                        },
                    }
                }

                // Update the tokens for this session
                self.tokens.push(tk);
                self.decoded_tokens.append(&mut token);
            }
        }
        log::trace!("Finished feed prompt");

        Ok(())
    }

    /// Removes `num` tokens from the end of the buffer. Roughly the inverse of `feed_prompt`.
    pub fn rewind(&mut self, model: &dyn Model, num: usize) -> Result<Vec<TokenId>, RewindError> {
        if !model.supports_rewind() {
            return Err(RewindError::UnsupportedArchitecture);
        }

        if num >= self.n_past {
            return Err(RewindError::NotEnoughTokens);
        }

        // Remove the tokens from self.tokens.
        let token_start = self.n_past - num;
        let deleted_tokens: Vec<_> = self.tokens.drain(token_start..).collect();

        // Remove the corresponding chars from decoded
        let mut decoded_start = self.decoded_tokens.len();
        for id in &deleted_tokens {
            decoded_start -= model.tokenizer().token(*id as usize).len();
        }
        self.decoded_tokens.truncate(decoded_start);

        // Decrement the n_past tokens counter.
        self.n_past -= num;

        Ok(deleted_tokens)
    }

    /// Infer the next token for this session.
    #[instrument(level = "trace", skip_all)]
    pub fn infer_next_token(
        &mut self,
        model: &dyn Model,
        params: &InferenceParameters,
        output_request: &mut OutputRequest,
        rng: &mut impl rand::Rng,
    ) -> Result<Vec<u8>, InferenceError> {
        if self.n_past + 1 >= model.context_size() {
            return Err(InferenceError::ContextFull);
        }

        let next_token = params.sampler.sample(&self.tokens, &self.last_logits, rng);

        // Update the tokens for this session
        self.tokens.push(next_token);

        // Then, evaluate the network again to compute the new last_logits
        model.evaluate(self, &[next_token], output_request);

        // Return the next token
        if next_token as TokenId == model.eot_token_id() {
            Err(InferenceError::EndOfText)
        } else {
            let res = match model.tokenizer() {
                crate::Tokenizer::Embedded(_) => {
                    model.tokenizer().token(next_token as usize).to_vec()
                }
                crate::Tokenizer::HuggingFace(_) => get_newly_decoded_portion_huggingface(
                    model,
                    self.tokens.clone(),
                    &self.decoded_tokens,
                ),
            };

            self.decoded_tokens.append(&mut res.clone());
            Ok(res)
        }
    }

    /// Generate text by using the provided [Model] to evaluate the `prompt`.
    ///
    /// The `callback` is called with each new token until an end-of-text (EOT)
    /// token is encountered or the maximum number of tokens have been
    /// generated (specified by [InferenceRequest::maximum_token_count]).
    ///
    /// This is a wrapper around [Self::feed_prompt] and [Self::infer_next_token].
    #[instrument(skip_all)]
    pub fn infer<E: std::error::Error + Send + Sync + 'static>(
        &mut self,
        model: &dyn Model,
        rng: &mut impl rand::Rng,
        request: &InferenceRequest,
        output_request: &mut OutputRequest,
        mut callback: impl FnMut(InferenceResponse) -> Result<InferenceFeedback, E>,
    ) -> Result<InferenceStats, InferenceError> {
        let maximum_token_count = request.maximum_token_count.unwrap_or(usize::MAX);
        if request.play_back_previous_tokens {
            // "Play back" the existing tokens, so that loading from an inference snapshot works
            // as expected.
            let mut token_utf8_buf = TokenUtf8Buffer::new();
            for token_id in &self.tokens {
                // Buffer the token until it's valid UTF-8, then call the callback.
                if let Some(tokens) =
                    token_utf8_buf.push(&model.tokenizer().token(*token_id as usize))
                {
                    if let Err(e) = callback(InferenceResponse::SnapshotToken(tokens)) {
                        return Err(InferenceError::UserCallback(Box::new(e)));
                    }
                }
            }
        }
        log::trace!(
            "Starting inference request with max_token_count: {}",
            maximum_token_count
        );

        let mut stats = InferenceStats::default();
        let start_at = std::time::SystemTime::now();

        let parameters = request.parameters;

        // Feed the initial prompt through the transformer, to update its
        // context window with new data, if necessary.
        if !request.prompt.is_empty() {
            self.feed_prompt(
                model,
                request.prompt,
                output_request,
                feed_prompt_callback(&mut callback),
            )?;
        }
        stats.feed_prompt_duration = start_at.elapsed().unwrap();
        stats.prompt_tokens = self.n_past;

        // After the prompt is consumed, sample tokens by repeatedly calling
        // `infer_next_token`. We generate tokens until the model returns an
        // EndOfText token, or we run out of space in the context window,
        // or we reach the specified limit.
        let mut tokens_processed = 0;
        let mut token_utf8_buf = TokenUtf8Buffer::new();
        while tokens_processed < maximum_token_count {
            let token = match self.infer_next_token(model, parameters, &mut Default::default(), rng)
            {
                Ok(token) => token,
                Err(InferenceError::EndOfText) => break,
                Err(e) => return Err(e),
            };

            // Buffer the token until it's valid UTF-8, then call the callback.
            if let Some(tokens) = token_utf8_buf.push(&token) {
                match callback(InferenceResponse::InferredToken(tokens)) {
                    Err(e) => return Err(InferenceError::UserCallback(Box::new(e))),
                    Ok(f) => match f {
                        InferenceFeedback::Continue => (),
                        InferenceFeedback::Halt => break,
                    },
                }
            }

            tokens_processed += 1;
        }
        stats.predict_duration = start_at.elapsed().unwrap();
        stats.predict_tokens = self.n_past;

        Ok(stats)
    }

    /// Calculate perplexity over a given prompt, with a value reported for each
    /// chunk that has been processed.
    ///
    /// This will behave similarly to [Self::feed_prompt], including altering
    /// the state of the LM, but will not generate any tokens.
    pub fn perplexity<'a, P: Into<Prompt<'a>>>(
        &mut self,
        model: &dyn Model,
        prompt: P,
        mut perplexity_callback: impl FnMut(usize, f32),
    ) -> Result<(), TokenizationError> {
        // Implementation based on perplexity example of llama.cpp:
        // https://github.com/ggerganov/llama.cpp/blob/2d5db48371052087a83974abda3767d1aedec598/examples/perplexity/perplexity.cpp#L24
        let mut tokens = prompt.into().to_tokens(model.tokenizer(), true)?;

        let mut count = 0;

        // TODO: make this handle <context_size tokens
        let context_size = model.context_size();
        let n_chunk = tokens.len() / context_size;
        let n_vocab = model.tokenizer().len();
        let n_batch = self.config.n_batch;

        let mut nll = 0.0;

        for i in 0..n_chunk {
            let start = i * context_size;
            let end = (i + 1) * context_size;

            let num_batches = (context_size + n_batch - 1) / n_batch;

            let mut logits = vec![];

            for j in 0..num_batches {
                let mut output_request = OutputRequest {
                    all_logits: Some(vec![]),
                    ..Default::default()
                };

                let batch_start = start + j * n_batch;
                let batch_size = (end - batch_start).min(n_batch);

                // Save the original token at the start of the batch.
                let token_org = tokens[batch_start];

                // Replace the first token with the BOS token, if necessary.
                if j == 0 {
                    tokens[batch_start] = model.bot_token_id().unwrap_or(1);
                }

                model.evaluate(
                    self,
                    &tokens[batch_start..batch_start + batch_size],
                    &mut output_request,
                );

                // Restore the original token.
                tokens[batch_start] = token_org;

                // Append the logits to the list.
                logits.extend(output_request.all_logits.unwrap());
            }

            for j in 512.min(context_size / 2)..(context_size - 1) {
                let logits = &logits[j * n_vocab..(j + 1) * n_vocab];
                let probability = util::softmax(logits)[tokens[start + j + 1] as usize];
                nll += -probability.ln();

                count += 1;
            }

            perplexity_callback(i, (nll / count as f32).exp());
        }

        Ok(())
    }

    /// Obtains a serializable snapshot of the current inference status. This
    /// can be used to cache the state of the model and store them into a file.
    ///
    /// # Safety
    ///
    /// This function provides raw access to the underlying memory owned by the
    /// ggml context. While the provided `InferenceSnapshotRef` object is alive,
    /// no other methods for this model object should be called.
    pub unsafe fn get_snapshot(&mut self) -> InferenceSnapshotRef<'_> {
        let memory_k = unsafe {
            std::slice::from_raw_parts(self.memory_k.data() as *mut u8, self.memory_k.nbytes())
        };
        let memory_v = unsafe {
            std::slice::from_raw_parts(self.memory_v.data() as *mut u8, self.memory_v.nbytes())
        };

        InferenceSnapshotRef {
            npast: self.n_past,
            config: self.config,
            tokens: self.tokens.clone(),
            logits: self.last_logits.clone(),
            memory_k,
            memory_v,
        }
    }

    /// Creates an [InferenceSession] from a snapshot.
    pub fn from_snapshot(
        snapshot: InferenceSnapshot,
        model: &dyn Model,
    ) -> Result<Self, SnapshotError> {
        let mut session = model.start_session(snapshot.config);

        if session.memory_k.nbytes() != snapshot.memory_k.len()
            || session.memory_v.nbytes() != snapshot.memory_v.len()
        {
            return Err(SnapshotError::MemorySizeMismatch {
                self_size: session.memory_k.nbytes() + session.memory_v.nbytes(),
                input_size: snapshot.memory_k.len() + snapshot.memory_v.len(),
            });
        }

        // SAFETY: We have exclusive access to Session, which means no one else
        // should be touching the context's memory. We can write to it because
        // we already checked the size.
        unsafe {
            session.memory_k.write_data(&snapshot.memory_k);
            session.memory_v.write_data(&snapshot.memory_v);
        }

        session.n_past = snapshot.npast;
        session.tokens = snapshot.tokens;
        session.last_logits = snapshot.last_logits;

        Ok(session)
    }

    /// All tokens generated by this inference session
    pub fn tokens(&self) -> &[TokenId] {
        self.tokens.as_ref()
    }

    /// All decoded tokens generated by this inference session
    pub fn decoded_tokens(&self) -> &[u8] {
        self.decoded_tokens.as_ref()
    }
}

impl Drop for InferenceSession {
    fn drop(&mut self) {
        // If we are using an accelerator, we need to free the scratch memory.
        // The k/v memory is freed by the ctx0 destructor.
        ggml::accelerator::free_scratch();
    }
}

fn get_newly_decoded_portion_huggingface(
    model: &dyn Model,
    tokens: Vec<u32>,
    decoded_tokens: &[u8],
) -> Vec<u8> {
    let all_tokens = model.tokenizer().decode(tokens, true);
    // The bytes here come from a lossily-decoded String, so we need to convert it back to a String
    // to check if it ends with a replacement character.
    let all_tokens = unsafe { String::from_utf8_unchecked(all_tokens) };
    if all_tokens.ends_with('�') {
        // Return an empty vector: no valid text was generated from this token.
        return vec![];
    }
    all_tokens.as_bytes()[decoded_tokens.len()..].to_vec()
}

#[derive(Error, Debug)]
/// Errors encountered during the inference process.
pub enum InferenceError {
    #[error("a tokenization-related failure occurred")]
    /// A tokenization-related failure occurred.
    TokenizationFailed(#[from] TokenizationError),
    #[error("the context window is full")]
    /// The context window for the model is full.
    ContextFull,
    #[error("reached end of text")]
    /// The model has produced an end of text token, signalling that it thinks that the text should end here.
    ///
    /// Note that this error *can* be ignored and inference can continue, but the results are not guaranteed to be sensical.
    EndOfText,
    #[error("the user-specified callback returned an error")]
    /// The user-specified callback returned an error.
    UserCallback(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Error, Debug)]
/// Errors encountered during the snapshot process.
pub enum RewindError {
    /// Tried deleting more tokens than were available
    #[error("tried deleting more tokens than were available")]
    NotEnoughTokens,

    /// Model architecture does not support delete
    #[error("model architecture does not support deletes")]
    UnsupportedArchitecture,
}

#[derive(Error, Debug)]
/// Errors encountered during the snapshot process.
pub enum SnapshotError {
    /// Arbitrary I/O error.
    #[error("I/O error while reading or writing snapshot")]
    IO(#[from] std::io::Error),
    /// Mismatch between the snapshotted memory and the in-memory memory.
    #[error("could not read snapshot due to size mismatch (self={self_size}, input={input_size})")]
    MemorySizeMismatch {
        /// The size of the session memory in memory.
        self_size: usize,
        /// The size of the session memory in snapshot.
        input_size: usize,
    },
}

#[derive(serde::Serialize, Clone, PartialEq)]
/// A serializable snapshot of the inference process.
/// Can be created by calling [InferenceSession::get_snapshot].
///
/// If serializing, ensure that your serializer is binary-efficient.
/// This type contains a large array of bytes; traditional textual serializers
/// are likely to serialize this as an array of numbers at extreme cost.
// Keep in sync with [InferenceSession] and [InferenceSnapshot].
pub struct InferenceSnapshotRef<'a> {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    /// Parameters associated with the saved inference session.
    pub config: InferenceSessionConfig,
    /// All tokens generated by this inference session.
    pub tokens: Vec<TokenId>,
    /// The vector of logits that was produced after the last inference.
    pub logits: Vec<f32>,
    /// The contents of the 'key' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_k: &'a [u8],
    /// The contents of the 'value' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_v: &'a [u8],
}
impl InferenceSnapshotRef<'_> {
    /// Creates an owned [InferenceSnapshot] from this [InferenceSnapshotRef].
    ///
    /// The [ToOwned] trait is not used due to its blanket implementation for all [Clone] types.
    pub fn to_owned(&self) -> InferenceSnapshot {
        InferenceSnapshot {
            npast: self.npast,
            config: self.config,
            tokens: self.tokens.clone(),
            last_logits: self.logits.clone(),
            memory_k: self.memory_k.to_vec(),
            memory_v: self.memory_v.to_vec(),
        }
    }
}

/// A serializable snapshot of the inference process. Can be restored by calling
/// [InferenceSession::from_snapshot].
#[derive(serde::Deserialize, Clone, PartialEq)]
// Keep in sync with [InferenceSession] and [InferenceSnapshotRef].
pub struct InferenceSnapshot {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    /// Parameters associated with the saved inference session.
    pub config: InferenceSessionConfig,
    /// All tokens generated by this inference session.
    pub tokens: Vec<TokenId>,
    /// The vector of logits that was produced after the last inference.
    pub last_logits: Vec<f32>,
    /// The contents of the 'key' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_k: Vec<u8>,
    /// The contents of the 'value' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_v: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
/// Configuration for an inference session.
///
/// This is specified at the time of creation of an [InferenceSession],
/// and cannot be changed after the session has been created.
pub struct InferenceSessionConfig {
    /// The type of the memory K tensor.
    pub memory_k_type: ModelKVMemoryType,

    /// The type of the memory V tensor.
    pub memory_v_type: ModelKVMemoryType,

    /// Controls batch/chunk size for prompt ingestion in [InferenceSession::feed_prompt].
    ///
    /// This is the number of tokens that will be ingested at once. This is useful for
    /// trying to speed up the ingestion of prompts, as it allows for parallelization.
    /// However, you will be fundamentally limited by your machine's ability to evaluate
    /// the transformer model, so increasing the batch size will not always help.
    ///
    /// A reasonable default value is 8.
    pub n_batch: usize,
    /// The number of threads to use. This is dependent on your user's system,
    /// and should be selected accordingly.
    ///
    /// Note that you should aim for a value close to the number of physical cores
    /// on the system, as this will give the best performance. This means that, for
    /// example, on a 16-core system with hyperthreading, you should set this to 16.
    ///
    /// Also note that not all cores on a system are equal, and that you may need to
    /// experiment with this value to find the optimal value for your use case. For example,
    /// Apple Silicon and modern Intel processors have "performance" and "efficiency" cores,
    /// and you may want to only use the performance cores.
    ///
    /// A reasonable default value is 8, as most modern high-performance computers have
    /// 8 physical cores. Adjust to your needs.
    pub n_threads: usize,
}

impl Default for InferenceSessionConfig {
    fn default() -> Self {
        Self {
            memory_k_type: ModelKVMemoryType::Float16,
            memory_v_type: ModelKVMemoryType::Float16,
            n_batch: 8,
            n_threads: 8,
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// Settings specific to [InferenceSession::infer].
pub struct InferenceRequest<'a> {
    /// The prompt to feed to the model.
    pub prompt: Prompt<'a>,
    /// The parameters to use during this inference attempt.
    pub parameters: &'a InferenceParameters,
    /// Whether or not to call the callback with the previous tokens
    /// that were encountered in this session.
    ///
    /// You likely want to turn this on if you're using a session
    /// that has been rehydrated from a snapshot.
    pub play_back_previous_tokens: bool,
    /// The maximum number of tokens to generate.
    pub maximum_token_count: Option<usize>,
}

/// Statistics about the inference process.
#[derive(Serialize, Debug, Clone, Copy)]
pub struct InferenceStats {
    /// How long it took to feed the prompt.
    pub feed_prompt_duration: std::time::Duration,
    /// How many tokens the prompt was.
    pub prompt_tokens: usize,
    /// How long it took to predict new tokens.
    pub predict_duration: std::time::Duration,
    /// The number of predicted tokens.
    pub predict_tokens: usize,
}
impl Default for InferenceStats {
    fn default() -> Self {
        Self {
            feed_prompt_duration: std::time::Duration::from_secs(0),
            prompt_tokens: 0,
            predict_duration: std::time::Duration::from_secs(0),
            predict_tokens: 0,
        }
    }
}
impl Display for InferenceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let Self {
            feed_prompt_duration,
            prompt_tokens,
            predict_duration,
            predict_tokens,
        } = *self;

        let feed_prompt_duration = feed_prompt_duration.as_millis();
        let predict_duration = predict_duration.as_millis();
        let per_token_duration = if predict_tokens == 0 {
            0.0
        } else {
            predict_duration as f64 / predict_tokens as f64
        };

        writeln!(f, "feed_prompt_duration: {}ms", feed_prompt_duration)?;
        writeln!(f, "prompt_tokens: {}", prompt_tokens)?;
        writeln!(f, "predict_duration: {}ms", predict_duration)?;
        writeln!(f, "predict_tokens: {}", predict_tokens)?;
        write!(f, "per_token_duration: {:.3}ms", per_token_duration)
    }
}

/// Allowed types for the model memory K/V tensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelKVMemoryType {
    /// 16-bit float.
    Float16,
    /// 32-bit float.
    Float32,
}
impl From<ModelKVMemoryType> for ggml::Type {
    fn from(value: ModelKVMemoryType) -> Self {
        match value {
            ModelKVMemoryType::Float16 => ggml::Type::F16,
            ModelKVMemoryType::Float32 => ggml::Type::F32,
        }
    }
}

/// A response to an inference request, sent as the argument to the `callback`
/// argument of the [InferenceSession::infer] function.
pub enum InferenceResponse {
    /// A token from playing back a snapshot
    SnapshotToken(String),
    /// A token from the prompt that has been fed into the inference session
    PromptToken(String),
    /// A token that has been generated via inference
    InferredToken(String),
    /// The inference session has generated an end-of-text token
    EotToken,
}

/// Feedback from a caller to [InferenceSession::infer], sent as the return
/// value to the `callback` function.
pub enum InferenceFeedback {
    /// Continue inference
    Continue,
    /// Halt inference
    Halt,
}

/// Adapt an [InferenceResponse] callback so that it can be used in a call to
/// [InferenceSession::feed_prompt].
pub fn feed_prompt_callback<'a, E: std::error::Error + Send + Sync + 'static>(
    mut callback: impl FnMut(InferenceResponse) -> Result<InferenceFeedback, E> + 'a,
) -> impl FnMut(&[u8]) -> Result<InferenceFeedback, E> + 'a {
    let mut buffer = TokenUtf8Buffer::new();
    move |token| match buffer.push(token) {
        Some(tokens) => callback(InferenceResponse::PromptToken(tokens)),
        None => Ok(InferenceFeedback::Continue),
    }
}

/// An [InferenceResponse] callback that will halt inference when a `stop_sequence` is generated.
/// This callback is used in [InferenceSession::infer] in chat_mode.
pub fn conversation_inference_callback<'a, E: std::error::Error + Send + Sync + 'static>(
    stop_sequence: &'a str,
    mut callback: impl FnMut(String) + 'a,
) -> impl FnMut(InferenceResponse) -> Result<InferenceFeedback, E> + 'a {
    let mut stop_sequence_buf = String::new();
    move |resp| match resp {
        InferenceResponse::InferredToken(token) => {
            // We've generated a token, so we need to check if it's contained in the stop sequence.
            let mut buf = stop_sequence_buf.clone();
            buf.push_str(&token);

            if buf.starts_with(stop_sequence) {
                // We've generated the stop sequence, so we're done.
                // Note that this will contain the extra tokens that were generated after the stop sequence,
                // which may affect generation. This is non-ideal, but it's the best we can do without
                // modifying the model.
                stop_sequence_buf.clear();
                return Ok(InferenceFeedback::Halt);
            } else if stop_sequence.starts_with(&buf) {
                // We've generated a prefix of the stop sequence, so we need to keep buffering.
                stop_sequence_buf = buf;
                return Ok(InferenceFeedback::Continue);
            }

            // We've generated a token that isn't part of the stop sequence, so we can
            // pass it to the callback.
            stop_sequence_buf.clear();
            callback(buf);
            Ok(InferenceFeedback::Continue)
        }
        InferenceResponse::EotToken => Ok(InferenceFeedback::Halt),
        _ => Ok(InferenceFeedback::Continue),
    }
}

/// Create the memory K/V tensors for the inference-session.
fn kv_memory(
    context: &Context,
    config: &InferenceSessionConfig,
    use_gpu: bool,
    n_elements: usize,
) -> (Tensor, Tensor) {
    let memory_k = context
        .new_tensor_1d(config.memory_k_type.into(), n_elements)
        .set_name("memory_k");
    let memory_v = context
        .new_tensor_1d(config.memory_v_type.into(), n_elements)
        .set_name("memory_v");

    if use_gpu {
        // CUDA requires the K/V-Memory to be on the GPU but excluded from the scratch buffer.
        // For OpenCL this is a no-op.
        //
        // Note that these must be manually freed from the accelerator in the `InferenceSession`
        // destructor. This is because `offload_no_scratch` does not update the `offloaded_tensors`
        // map, because reasons.
        memory_k.offload_no_scratch();
        memory_v.offload_no_scratch();
    }

    (memory_k, memory_v)
}
