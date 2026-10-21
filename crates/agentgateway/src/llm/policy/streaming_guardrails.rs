//! Streaming guardrail infrastructure: `GuardedSseBody` and per-guardrail evaluators.
//!
//! `GuardedSseBody` implements **windowed guardrail evaluation**:
//!
//! 1. Incoming SSE byte frames are held (not forwarded) while text deltas are
//!    accumulated into a pending batch.
//! 2. When the batch reaches `eval_threshold` bytes of text (or the stream ends),
//!    all `StreamingEvaluator`s are run against a window consisting of an
//!    *overlap tail* from previously evaluated text plus the pending batch. The
//!    overlap ensures patterns spanning a batch boundary are still seen
//!    contiguously by at least one evaluation.
//! 3. **Pass** → the held frames are flushed to the client and buffering resumes.
//!    **Block** → the held (never-forwarded) frames are discarded and a synthetic
//!    SSE error event is emitted. Content flushed by earlier passing windows
//!    cannot be retracted — an accepted accuracy/latency tradeoff.
//!
//! This is not 100% accurate: a guard that needs full-response context, or a
//! pattern spanning more than the overlap window, can be missed.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use ::http::HeaderMap;
use bytes::Bytes;
use http_body::Frame;
use pin_project_lite::pin_project;
use tokio_sse_codec::{Event, Frame as SseFrame, SseDecoder};
use tokio_util::codec::Decoder;
use tracing::warn;

use super::{
	FailureMode, ResponseGuard, ResponseGuardKind, StreamFinisher, StreamSummary, StreamingEvaluator,
	StreamingGuardrailOutcome, WindowMeta,
};
use crate::cel::RequestSnapshot;
use crate::llm::policy::{Policy, PromptGuard};
use crate::proxy::httpproxy::PolicyClient;
use crate::telemetry::log::GuardrailLog;
use crate::telemetry::metrics::{GuardrailAction, GuardrailPhase};

/// Text bytes accumulated before triggering a guardrail evaluation.
/// Larger values reduce guardrail API calls but increase time-to-first-byte
/// and the amount of content discarded on a mid-stream block.
pub const DEFAULT_EVAL_THRESHOLD: usize = 1024;

/// Tail bytes of previously evaluated text prepended to each new window so
/// patterns spanning a batch boundary are still seen contiguously.
pub const OVERLAP_BYTES: usize = 256;

/// Return the last `max_bytes` of `s`, respecting UTF-8 char boundaries.
pub fn tail_chars(s: &str, max_bytes: usize) -> &str {
	if s.len() <= max_bytes {
		return s;
	}
	let mut start = s.len() - max_bytes;
	while !s.is_char_boundary(start) {
		start += 1;
	}
	&s[start..]
}

/// Run all evaluators against a window. Returns the rejection body if any evaluator blocked.
pub async fn evaluate_window(
	evaluators: &mut [Box<dyn StreamingEvaluator>],
	window: &str,
) -> Option<Bytes> {
	evaluate_window_at(evaluators, window, WindowMeta::default()).await
}

/// Run all evaluators against a window at a known position in the stream.
pub async fn evaluate_window_at(
	evaluators: &mut [Box<dyn StreamingEvaluator>],
	window: &str,
	meta: WindowMeta,
) -> Option<Bytes> {
	for ev in evaluators.iter_mut() {
		match ev.evaluate_window(window, meta.clone()).await {
			Ok(Some(StreamingGuardrailOutcome::Blocked(body))) => {
				tracing::debug!("streaming guardrail blocked response window");
				return Some(body);
			},
			Ok(None) => {},
			Err(e) => match ev.failure_mode() {
				FailureMode::FailClosed => {
					warn!("streaming guardrail error, failing closed: {e}");
					return Some(Bytes::from_static(b"Content blocked by guardrail policy"));
				},
				FailureMode::FailOpen => {
					warn!("streaming guardrail error, failing open: {e}");
				},
			},
		}
	}
	None
}

/// The engine's usage for a streamed answer, read when the stream ends and priced with the catalog.
pub struct StreamUsage {
	log: crate::telemetry::log::AsyncLog<agent_llm::LLMInfo>,
	catalog: Option<Arc<crate::llm::catalog::ModelCatalog>>,
}

impl StreamUsage {
	pub fn new(
		log: crate::telemetry::log::AsyncLog<agent_llm::LLMInfo>,
		catalog: Option<Arc<crate::llm::catalog::ModelCatalog>>,
	) -> Self {
		Self { log, catalog }
	}

	/// The usage and cost as the engine has them now; absent when nothing was logged.
	fn context(&self) -> Option<serde_json::Value> {
		let mut ctx = None;
		self.log.non_atomic_mutate(|info| {
			ctx = super::usage_value(info.clone(), self.catalog.as_deref());
		});
		ctx
	}
}

/// Run the stream finishers once, for a stream whose end was not reported by its last window: a
/// client abort (including one while that window's call is in flight), or a guard block on an
/// earlier window. They were captured at construction, so this works whether the evaluators are
/// at rest or moved into an evaluation future at the moment the body ends. `windows` is the
/// position the call takes in the stream: the index the last window had, or the next index when
/// no last window was started, so a receiver keys every call by its position.
fn finish_stream(
	finishers: &[StreamFinisher],
	usage: Option<&StreamUsage>,
	windows: u32,
	finished: &mut bool,
) {
	if std::mem::replace(finished, true) || finishers.is_empty() {
		return;
	}
	let usage = usage.and_then(StreamUsage::context);
	for finisher in finishers {
		finisher(StreamSummary {
			windows,
			stream: "text",
			usage: usage.clone(),
		});
	}
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Construct a boxed `StreamingEvaluator` for the given `ResponseGuard`.
pub fn make_evaluator(
	guard: &ResponseGuard,
	client: PolicyClient,
	http_headers: HeaderMap,
	original: Option<Arc<RequestSnapshot>>,
	guardrail_log: GuardrailLog,
) -> Box<dyn StreamingEvaluator> {
	Box::new(ResponseGuardEvaluator {
		guard: guard.clone(),
		client,
		http_headers,
		original,
		guardrail_log,
		worst_action: GuardrailAction::Allow,
		audit_recorded: false,
		allow_recorded: false,
		fail_open_recorded: false,
		window_meta: None,
	})
}

struct ResponseGuardEvaluator {
	guard: ResponseGuard,
	client: PolicyClient,
	http_headers: HeaderMap,
	original: Option<Arc<RequestSnapshot>>,
	guardrail_log: GuardrailLog,
	// Fold window results into one metric for the stream.
	worst_action: GuardrailAction,
	// Only log the audit action once per stream, even if triggered by multiple windows.
	audit_recorded: bool,
	// Deduplicate passing windows
	allow_recorded: bool,
	fail_open_recorded: bool,
	// The position of the window being evaluated, set by `evaluate_window` for `evaluate`.
	window_meta: Option<WindowMeta>,
}

impl ResponseGuardEvaluator {
	fn observe_action(&mut self, action: GuardrailAction) {
		self.worst_action = self.worst_action.max(action);
	}

	/// Only a webhook with `includeBody` wants the stream's end; the other guard kinds have nothing
	/// to do with usage.
	fn wants_stream_end(&self) -> bool {
		matches!(&self.guard.kind, ResponseGuardKind::Webhook(wh) if wh.include_body)
	}
}

impl Drop for ResponseGuardEvaluator {
	fn drop(&mut self) {
		Policy::record_guardrail_trip(&self.client, GuardrailPhase::Response, self.worst_action);
	}
}

#[async_trait::async_trait]
impl StreamingEvaluator for ResponseGuardEvaluator {
	fn failure_mode(&self) -> FailureMode {
		self.guard.failure_mode()
	}

	async fn evaluate_window(
		&mut self,
		window: &str,
		meta: WindowMeta,
	) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
		// An empty last window exists only for the end-of-stream call.
		if window.is_empty() && !(meta.is_final && self.wants_stream_end()) {
			return Ok(None);
		}
		self.window_meta = Some(meta);
		self.evaluate(window).await
	}

	fn stream_finisher(&self) -> Option<StreamFinisher> {
		if !self.wants_stream_end() {
			return None;
		}
		let ResponseGuardKind::Webhook(wh) = &self.guard.kind else {
			return None;
		};
		let (client, wh, headers, original) = (
			self.client.clone(),
			wh.clone(),
			self.http_headers.clone(),
			self.original.clone(),
		);
		Some(Arc::new(move |summary: StreamSummary| {
			super::webhook::spawn_final_response(
				client.clone(),
				wh.clone(),
				headers.clone(),
				original.clone(),
				super::webhook::ResponseExtras {
					usage: summary.usage,
					index: Some(summary.windows),
					stream: Some(summary.stream),
					is_final: true,
				},
			);
		}))
	}

	async fn evaluate(&mut self, window: &str) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
		let log = if self.audit_recorded {
			None
		} else {
			Some(&self.guardrail_log)
		};

		match PromptGuard::evaluate_streaming_response_window(
			&self.guard,
			window,
			&self.client,
			&self.http_headers,
			self.original.as_deref(),
			log,
			&mut self.allow_recorded,
			self.window_meta.take(),
		)
		.await
		{
			Ok((outcome, action)) => {
				if action == GuardrailAction::Audit {
					self.audit_recorded = true;
				}
				self.observe_action(action);
				Ok(outcome)
			},
			Err(e) => {
				let action = match self.failure_mode() {
					FailureMode::FailClosed => GuardrailAction::Reject,
					FailureMode::FailOpen => GuardrailAction::FailOpen,
				};
				if action != GuardrailAction::FailOpen || !self.fail_open_recorded {
					super::record_guardrail(
						Some(&self.guardrail_log),
						GuardrailPhase::Response,
						self.guard.kind.name(),
						action,
						None,
					);
					self.fail_open_recorded |= action == GuardrailAction::FailOpen;
				}
				self.observe_action(action);
				Err(e)
			},
		}
	}
}

// ---------------------------------------------------------------------------
// GuardedSseBody
// ---------------------------------------------------------------------------

/// Synthetic SSE error event sent to the client when a guardrail blocks content.
///
/// Encoded as a single `data: {...}\n\n` SSE frame.
fn guardrail_blocked_sse_bytes(body: Bytes) -> Bytes {
	let message = String::from_utf8_lossy(&body);
	let event = serde_json::json!({
		"error": {
			"type": "guardrail_blocked",
			"code": "guardrail_intervention",
			"message": message,
		}
	});
	Bytes::from(format!("data: {event}\n\n"))
}

type EvalFuture =
	Pin<Box<dyn Future<Output = (Vec<Box<dyn StreamingEvaluator>>, Option<Bytes>)> + Send + 'static>>;

/// Internal state machine for `GuardedSseBody`.
enum GuardedBodyState {
	/// Reading from upstream, holding frames until the eval threshold is reached.
	Buffering,
	/// Evaluating the current window asynchronously. `eof` records whether the
	/// upstream is already exhausted (this is the final evaluation).
	Evaluating { fut: EvalFuture, eof: bool },
	/// Yield held frames in order, then return to `Buffering` (or `Done` if `eof`).
	Flushing { queue: VecDeque<Bytes>, eof: bool },
	/// Send the synthetic error event then close.
	Blocked(Bytes),
	/// Done – no more frames.
	Done,
}

pin_project! {
	// An `http_body::Body` wrapper that implements windowed guardrail evaluation.
	pub struct GuardedSseBody {
		#[pin]
		inner: agent_http::RawBody,
		evaluators: Vec<Box<dyn StreamingEvaluator>>,
		eval_threshold: usize,
		buffer_limit: usize,
		held_frames: Vec<Bytes>,
		held_bytes: usize,
		pending_text: String,
		overlap_tail: String,
		sse_decoder: SseDecoder<Bytes>,
		decode_buffer: bytes::BytesMut,
		state: GuardedBodyState,
		// Owns the rate-limit logger; dropped only when this body is fully consumed,
		// so telemetry is recorded at the correct time.
		logger: Option<crate::llm::AmendOnDrop>,
		// Windows evaluated so far; the next window's index.
		window_index: u32,
		// Where to read the usage from when the stream ends, for the finishers.
		usage: Option<StreamUsage>,
		// What to call when the stream ends, captured from the evaluators at construction.
		finishers: Vec<StreamFinisher>,
		// `finish` has run, on completion or on drop.
		finished: bool,
	}

	impl PinnedDrop for GuardedSseBody {
		fn drop(this: Pin<&mut Self>) {
			let this = this.project();
			finish_stream(
				this.finishers,
				this.usage.as_ref(),
				*this.window_index,
				this.finished,
			);
		}
	}
}

impl GuardedSseBody {
	/// Create a new `GuardedSseBody` with the default evaluation threshold.
	///
	/// * `inner` – the upstream SSE body.
	/// * `evaluators` – one evaluator per configured response guard.
	/// * `buffer_limit` – max bytes of held frames; reaching it forces an evaluation.
	/// * `logger` – rate-limit logger that must outlive the streaming response.
	// We do actually return Self; just wrapped in an http_body::Body. The annotation silences a false positive from clippy about that.
	#[allow(clippy::new_ret_no_self)]
	pub fn new(
		inner: agent_http::RawBody,
		evaluators: Vec<Box<dyn StreamingEvaluator>>,
		buffer_limit: usize,
		logger: Option<crate::llm::AmendOnDrop>,
	) -> agent_http::RawBody {
		Self::with_threshold(
			inner,
			evaluators,
			buffer_limit,
			logger,
			DEFAULT_EVAL_THRESHOLD,
		)
	}

	/// Like [`GuardedSseBody::new`] but with an explicit evaluation threshold.
	pub fn with_threshold(
		inner: agent_http::RawBody,
		evaluators: Vec<Box<dyn StreamingEvaluator>>,
		buffer_limit: usize,
		logger: Option<crate::llm::AmendOnDrop>,
		eval_threshold: usize,
	) -> agent_http::RawBody {
		Self::with_usage(
			inner,
			evaluators,
			buffer_limit,
			logger,
			eval_threshold,
			None,
		)
	}

	/// Like [`GuardedSseBody::with_threshold`], reading the engine's usage for this stream when it
	/// ends, for the evaluators' `finish`.
	pub fn with_usage(
		inner: agent_http::RawBody,
		evaluators: Vec<Box<dyn StreamingEvaluator>>,
		buffer_limit: usize,
		logger: Option<crate::llm::AmendOnDrop>,
		eval_threshold: usize,
		usage: Option<StreamUsage>,
	) -> agent_http::RawBody {
		let finishers = if usage.is_some() {
			evaluators
				.iter()
				.filter_map(|ev| ev.stream_finisher())
				.collect()
		} else {
			Vec::new()
		};
		agent_http::RawBody::new(Self {
			inner,
			evaluators,
			eval_threshold,
			buffer_limit,
			held_frames: Vec::new(),
			held_bytes: 0,
			pending_text: String::new(),
			overlap_tail: String::new(),
			sse_decoder: SseDecoder::with_max_size(buffer_limit),
			decode_buffer: bytes::BytesMut::new(),
			state: GuardedBodyState::Buffering,
			logger,
			window_index: 0,
			usage,
			finishers,
			finished: false,
		})
	}

	/// Extract text delta from a parsed SSE frame if present.
	fn extract_text_delta(frame: SseFrame<Bytes>) -> Option<String> {
		let SseFrame::Event(Event { data, .. }) = frame else {
			return None;
		};
		if data.as_ref() == b"[DONE]" {
			return None;
		}
		if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
			// OpenAI responses: response.output_text.delta
			if v.get("type").and_then(|t| t.as_str()) == Some("response.output_text.delta")
				&& let Some(text) = v.get("delta").and_then(|s| s.as_str())
			{
				return Some(text.to_string());
			}
			// OpenAI completions: choices[0].delta.content
			if let Some(text) = v
				.get("choices")
				.and_then(|c| c.get(0))
				.and_then(|c| c.get("delta"))
				.and_then(|d| d.get("content"))
				.and_then(|s| s.as_str())
			{
				return Some(text.to_string());
			}
			// Anthropic messages: delta.text
			if let Some(text) = v
				.get("delta")
				.and_then(|d| d.get("text"))
				.and_then(|s| s.as_str())
			{
				return Some(text.to_string());
			}
			// Native Gemini: candidates[].content.parts[].text. `candidateCount` is client
			// controlled, so reading only candidates[0] would let the client hide text from the
			// guard in a second candidate. Concatenating every candidate is the conservative
			// choice: the guard evaluates one text stream, and a window that contains all
			// candidates can only match more than one that contains a subset — every substring of
			// a single candidate is still contiguous in the concatenation. Thought parts are
			// excluded, as they are on the non-streaming path (types::gemini::candidate_text).
			if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
				return Some(
					candidates
						.iter()
						.filter_map(|c| {
							c.get("content")
								.and_then(|c| c.get("parts"))
								.and_then(|p| p.as_array())
						})
						.flatten()
						.filter(|p| p.get("thought").and_then(serde_json::Value::as_bool) != Some(true))
						.filter_map(|p| p.get("text").and_then(|t| t.as_str()))
						.collect(),
				);
			}
		}
		None
	}
}

impl http_body::Body for GuardedSseBody {
	type Data = Bytes;
	type Error = crate::http::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		let mut this = self.project();

		loop {
			match this.state {
				// -----------------------------------------------------------------
				// Flushing: yield held frames one at a time.
				// -----------------------------------------------------------------
				GuardedBodyState::Flushing { queue, eof } => {
					if let Some(frame) = queue.pop_front() {
						return Poll::Ready(Some(Ok(Frame::data(frame))));
					} else if *eof {
						*this.state = GuardedBodyState::Done;
						finish_stream(
							this.finishers,
							this.usage.as_ref(),
							*this.window_index,
							this.finished,
						);
						return Poll::Ready(None);
					} else {
						*this.state = GuardedBodyState::Buffering;
					}
				},
				// -----------------------------------------------------------------
				// Blocked: yield synthetic error event, then done.
				// -----------------------------------------------------------------
				GuardedBodyState::Blocked(body) => {
					let body = std::mem::take(body);
					*this.state = GuardedBodyState::Done;
					finish_stream(
						this.finishers,
						this.usage.as_ref(),
						*this.window_index,
						this.finished,
					);
					return Poll::Ready(Some(Ok(Frame::data(guardrail_blocked_sse_bytes(body)))));
				},
				// -----------------------------------------------------------------
				// Done: stream exhausted.
				// -----------------------------------------------------------------
				GuardedBodyState::Done => {
					return Poll::Ready(None);
				},
				// -----------------------------------------------------------------
				// Evaluating: poll the guardrail future for the current window.
				// -----------------------------------------------------------------
				GuardedBodyState::Evaluating { fut, eof } => match fut.as_mut().poll(cx) {
					Poll::Pending => return Poll::Pending,
					Poll::Ready((evaluators, blocked_body)) => {
						*this.evaluators = evaluators;
						if *eof {
							// The last window's call completed, allowed or blocked: the end is reported
							// and the drop and flush paths must not report it again.
							*this.finished = true;
						}
						if let Some(body) = blocked_body {
							this.held_frames.clear();
							*this.held_bytes = 0;
							*this.state = GuardedBodyState::Blocked(body);
						} else {
							let queue: VecDeque<Bytes> = this.held_frames.drain(..).collect();
							*this.held_bytes = 0;
							*this.state = GuardedBodyState::Flushing { queue, eof: *eof };
						}
					},
				},
				// -----------------------------------------------------------------
				// Buffering: read from upstream, holding frames until threshold.
				// -----------------------------------------------------------------
				GuardedBodyState::Buffering => {
					match this.inner.as_mut().poll_frame(cx) {
						Poll::Pending => return Poll::Pending,
						Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
						Poll::Ready(Some(Ok(frame))) => {
							let Some(data) = frame.data_ref() else {
								return Poll::Ready(Some(Ok(frame)));
							};

							let raw = data.clone();
							*this.held_bytes += raw.len();
							this.held_frames.push(raw.clone());

							this.decode_buffer.extend_from_slice(&raw);
							loop {
								match this.sse_decoder.decode(this.decode_buffer) {
									Ok(Some(sse_frame)) => {
										if let Some(delta) = GuardedSseBody::extract_text_delta(sse_frame) {
											this.pending_text.push_str(&delta);
										}
									},
									Ok(None) => break,
									Err(e) => {
										// Clear the buffer and reset the decoder so invalid bytes
										// don't accumulate across chunks and cause repeated errors.
										warn!("SSE decode error in streaming guardrail body, resetting decoder: {e}");
										this.decode_buffer.clear();
										*this.sse_decoder = SseDecoder::with_max_size(*this.buffer_limit);
										break;
									},
								}
							}

							let over_limit = *this.held_bytes >= *this.buffer_limit;
							if this.pending_text.len() >= *this.eval_threshold || over_limit {
								// Having a full buffer but empty text implies that the
								// buffer is full of non-text frames (e.g. control frames or unsupported SSE formats that fail to decode).
								// In that case, flush the buffer as-is without evaluation, to avoid stalling on unprocessable content.
								if this.pending_text.is_empty() {
									let queue: VecDeque<Bytes> = this.held_frames.drain(..).collect();
									*this.held_bytes = 0;
									*this.state = GuardedBodyState::Flushing { queue, eof: false };
									continue;
								}
								let batch = std::mem::take(this.pending_text);
								let window = format!("{}{}", this.overlap_tail, batch);
								*this.overlap_tail = tail_chars(&window, OVERLAP_BYTES).to_string();
								let mut evaluators = std::mem::take(this.evaluators);
								let meta = WindowMeta {
									index: *this.window_index,
									stream: "text",
									..Default::default()
								};
								*this.window_index += 1;
								let fut: EvalFuture = Box::pin(async move {
									let blocked_body = evaluate_window_at(&mut evaluators, &window, meta).await;
									(evaluators, blocked_body)
								});
								*this.state = GuardedBodyState::Evaluating { fut, eof: false };
							}
						},
						Poll::Ready(None) => {
							loop {
								match this.sse_decoder.decode_eof(this.decode_buffer) {
									Ok(Some(sse_frame)) => {
										if let Some(delta) = GuardedSseBody::extract_text_delta(sse_frame) {
											this.pending_text.push_str(&delta);
										}
									},
									Ok(None) => break,
									Err(e) => {
										warn!("SSE decode error at EOF in streaming guardrail body: {e}");
										this.decode_buffer.clear();
										break;
									},
								}
							}

							// The last window is evaluated while its frames are still held, so a webhook
							// that wants the stream's end gets it synchronously, with the usage the engine
							// has parsed by now. Without such a webhook an empty tail is released as before.
							if this.pending_text.is_empty() && this.finishers.is_empty() {
								let queue: VecDeque<Bytes> = this.held_frames.drain(..).collect();
								*this.held_bytes = 0;
								*this.state = GuardedBodyState::Flushing { queue, eof: true };
								continue;
							}

							let batch = std::mem::take(this.pending_text);
							// No pending text means no window to judge: the last call carries nothing,
							// never the overlap of text the client already has.
							let window = if batch.is_empty() {
								String::new()
							} else {
								format!("{}{}", this.overlap_tail, batch)
							};
							this.overlap_tail.clear();
							let mut evaluators = std::mem::take(this.evaluators);
							let usage = (!this.finishers.is_empty())
								.then(|| this.usage.as_ref().and_then(StreamUsage::context))
								.flatten();
							// The last window keeps `window_index` as its position: if the client drops
							// the body while its call is in flight, the drop path reports the end at the
							// same index, and the receiver can treat the two as one.
							let meta = WindowMeta {
								index: *this.window_index,
								stream: "text",
								is_final: true,
								usage,
							};
							let fut: EvalFuture = Box::pin(async move {
								let blocked_body = evaluate_window_at(&mut evaluators, &window, meta).await;
								(evaluators, blocked_body)
							});
							*this.state = GuardedBodyState::Evaluating { fut, eof: true };
						},
					}
				},
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use http_body_util::BodyExt as _;

	use super::*;

	struct PassEvaluator;

	#[async_trait::async_trait]
	impl StreamingEvaluator for PassEvaluator {
		async fn evaluate(
			&mut self,
			_window: &str,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			Ok(None)
		}
	}

	struct BlockEvaluator;

	#[async_trait::async_trait]
	impl StreamingEvaluator for BlockEvaluator {
		async fn evaluate(
			&mut self,
			_window: &str,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			Ok(Some(StreamingGuardrailOutcome::Blocked(
				Bytes::from_static(b"blocked"),
			)))
		}
	}

	struct PatternEvaluator {
		pattern: regex::Regex,
	}

	#[async_trait::async_trait]
	impl StreamingEvaluator for PatternEvaluator {
		async fn evaluate(
			&mut self,
			window: &str,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			if self.pattern.is_match(window) {
				return Ok(Some(StreamingGuardrailOutcome::Blocked(
					Bytes::from_static(b"blocked"),
				)));
			}
			Ok(None)
		}
	}

	struct ErrorEvaluator {
		mode: crate::llm::policy::FailureMode,
	}

	#[async_trait::async_trait]
	impl StreamingEvaluator for ErrorEvaluator {
		fn failure_mode(&self) -> crate::llm::policy::FailureMode {
			self.mode
		}

		async fn evaluate(
			&mut self,
			_window: &str,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			Err(anyhow::anyhow!("simulated evaluator error"))
		}
	}

	fn sse_bytes(content: &str) -> Bytes {
		Bytes::from(format!("data: {}\n\n", content))
	}

	fn delta_bytes(text: &str) -> Bytes {
		sse_bytes(&format!(
			"{{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}",
			text
		))
	}

	fn make_body(chunks: Vec<Bytes>) -> agent_http::RawBody {
		use std::convert::Infallible;

		use futures_util::stream;
		let stream = stream::iter(chunks.into_iter().map(Ok::<Bytes, Infallible>));
		agent_http::RawBody::from_stream(stream)
	}

	fn contains(haystack: &[u8], needle: &[u8]) -> bool {
		haystack.windows(needle.len()).any(|w| w == needle)
	}

	/// Records every window it is handed, and the position of each detached stream-end call.
	/// `on_final` decides what the last window does: pass, block, or never answer.
	struct RecordingEvaluator {
		seen: Seen,
		ended: Ended,
		on_final: OnFinal,
	}

	#[derive(Clone, Copy)]
	enum OnFinal {
		Pass,
		Block,
		Hang,
	}

	#[async_trait::async_trait]
	impl StreamingEvaluator for RecordingEvaluator {
		async fn evaluate(
			&mut self,
			_window: &str,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			Ok(None)
		}

		async fn evaluate_window(
			&mut self,
			window: &str,
			meta: WindowMeta,
		) -> anyhow::Result<Option<StreamingGuardrailOutcome>> {
			let is_final = meta.is_final;
			self.seen.lock().unwrap().push((window.to_string(), meta));
			match self.on_final {
				OnFinal::Block if is_final => Ok(Some(StreamingGuardrailOutcome::Blocked(
					Bytes::from_static(b"blocked at the end"),
				))),
				OnFinal::Hang if is_final => futures_util::future::pending().await,
				_ => Ok(None),
			}
		}

		fn stream_finisher(&self) -> Option<StreamFinisher> {
			let ended = self.ended.clone();
			Some(Arc::new(move |summary: StreamSummary| {
				ended.lock().unwrap().push(summary.windows);
			}))
		}
	}

	type Seen = Arc<std::sync::Mutex<Vec<(String, WindowMeta)>>>;
	type Ended = Arc<std::sync::Mutex<Vec<u32>>>;

	fn recording() -> (Box<dyn StreamingEvaluator>, Seen, Ended) {
		recording_with(OnFinal::Pass)
	}

	fn recording_with(on_final: OnFinal) -> (Box<dyn StreamingEvaluator>, Seen, Ended) {
		let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
		let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
		let evaluator = RecordingEvaluator {
			seen: seen.clone(),
			ended: ended.clone(),
			on_final,
		};
		(Box::new(evaluator), seen, ended)
	}

	fn guarded_with_usage(
		body: agent_http::RawBody,
		evaluator: Box<dyn StreamingEvaluator>,
	) -> agent_http::RawBody {
		GuardedSseBody::with_usage(
			body,
			vec![evaluator],
			1024 * 1024,
			None,
			DEFAULT_EVAL_THRESHOLD,
			Some(stream_usage()),
		)
	}

	/// When the last text frame itself crossed the threshold, nothing is pending at the end: the
	/// final window is empty, not the overlap of text the client already has.
	#[tokio::test]
	async fn the_final_window_is_empty_when_the_last_text_was_already_judged() {
		let (evaluator, seen, ended) = recording();
		let text = "x".repeat(DEFAULT_EVAL_THRESHOLD + 50);
		let body = make_body(vec![
			delta_bytes(&text),
			sse_bytes("{\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}"),
			sse_bytes("[DONE]"),
		]);
		let out = guarded_with_usage(body, evaluator)
			.collect()
			.await
			.unwrap()
			.to_bytes();
		assert!(contains(&out, b"[DONE]"));
		let seen = seen.lock().unwrap();
		assert_eq!(seen.len(), 2);
		assert_eq!(seen[0].0.len(), text.len());
		assert!(!seen[0].1.is_final);
		assert_eq!(seen[1].0, "", "the final window carries no overlap");
		assert!(seen[1].1.is_final);
		assert_eq!(seen[1].1.index, 1);
		assert!(ended.lock().unwrap().is_empty());
	}

	/// A client that drops the body while the final window's call is in flight still gets the end
	/// reported, once, at the same position the final window had.
	#[tokio::test]
	async fn an_abort_during_the_final_call_reports_the_end_at_the_same_index() {
		let (evaluator, seen, ended) = recording_with(OnFinal::Hang);
		let body = make_body(vec![delta_bytes("hi"), sse_bytes("[DONE]")]);
		let mut guarded = guarded_with_usage(body, evaluator);
		let waited = tokio::time::timeout(std::time::Duration::from_millis(50), guarded.frame()).await;
		assert!(waited.is_err(), "the final call never answers");
		drop(guarded);
		let seen = seen.lock().unwrap();
		assert_eq!(seen.len(), 1);
		assert!(seen[0].1.is_final);
		assert_eq!(*ended.lock().unwrap(), vec![seen[0].1.index]);
	}

	/// A block on the final window withholds the held tail and reports nothing more: the final
	/// window's own call already carried the end.
	#[tokio::test]
	async fn a_block_on_the_final_window_withholds_the_tail_and_sends_no_second_call() {
		let (evaluator, seen, ended) = recording_with(OnFinal::Block);
		let body = make_body(vec![delta_bytes("hi"), sse_bytes("[DONE]")]);
		let out = guarded_with_usage(body, evaluator)
			.collect()
			.await
			.unwrap()
			.to_bytes();
		assert!(!contains(&out, b"[DONE]"));
		assert!(contains(&out, b"blocked at the end"));
		assert_eq!(seen.lock().unwrap().len(), 1);
		assert!(ended.lock().unwrap().is_empty());
	}

	fn stream_usage() -> StreamUsage {
		StreamUsage::new(crate::telemetry::log::AsyncLog::default(), None)
	}

	/// A stream that reaches its end reports it in the last window, synchronously and before the
	/// held tail is released; the detached finisher does not run.
	#[tokio::test]
	async fn the_last_window_is_final_and_the_finisher_does_not_run_after_it() {
		let (evaluator, seen, ended) = recording();
		let body = make_body(vec![delta_bytes("hi"), sse_bytes("[DONE]")]);
		let guarded = GuardedSseBody::with_usage(
			body,
			vec![evaluator],
			1024 * 1024,
			None,
			DEFAULT_EVAL_THRESHOLD,
			Some(stream_usage()),
		);
		let out = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&out, b"[DONE]"));
		let seen = seen.lock().unwrap();
		assert_eq!(seen.len(), 1);
		assert_eq!(seen[0].0, "hi");
		assert!(seen[0].1.is_final);
		assert_eq!(seen[0].1.index, 0);
		assert!(ended.lock().unwrap().is_empty());
	}

	/// A stream with no text at all still ends with one empty final window when a finisher exists.
	#[tokio::test]
	async fn a_stream_without_text_still_gets_an_empty_final_window() {
		let (evaluator, seen, ended) = recording();
		let body = make_body(vec![sse_bytes(
			"{\"choices\":[{\"delta\":{\"tool_calls\":[]}}]}",
		)]);
		let guarded = GuardedSseBody::with_usage(
			body,
			vec![evaluator],
			1024 * 1024,
			None,
			DEFAULT_EVAL_THRESHOLD,
			Some(stream_usage()),
		);
		guarded.collect().await.unwrap();
		let seen = seen.lock().unwrap();
		assert_eq!(seen.len(), 1);
		assert_eq!(seen[0].0, "");
		assert!(seen[0].1.is_final);
		assert!(ended.lock().unwrap().is_empty());
	}

	/// A client that drops the stream before its end gets the detached finisher, once.
	#[tokio::test]
	async fn an_abandoned_stream_runs_the_finisher_once() {
		use futures_util::{StreamExt as _, stream};
		let (evaluator, seen, ended) = recording();
		// One small delta, then an upstream that never ends: the frame is held below the threshold.
		let stream = stream::iter(vec![Ok::<Bytes, std::convert::Infallible>(delta_bytes(
			"hi",
		))])
		.chain(stream::pending());
		let body = agent_http::RawBody::from_stream(stream);
		let mut guarded = GuardedSseBody::with_usage(
			body,
			vec![evaluator],
			1024 * 1024,
			None,
			DEFAULT_EVAL_THRESHOLD,
			Some(stream_usage()),
		);
		// The client gives up waiting and drops the body.
		let waited = tokio::time::timeout(std::time::Duration::from_millis(50), guarded.frame()).await;
		assert!(waited.is_err(), "nothing is released below the threshold");
		drop(guarded);
		assert!(seen.lock().unwrap().is_empty());
		assert_eq!(
			*ended.lock().unwrap(),
			vec![0],
			"one call, at the next position"
		);
	}

	#[tokio::test]
	async fn test_pass_through() {
		let chunk = delta_bytes("hello");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk.clone(), done.clone()]);

		let guarded = GuardedSseBody::new(body, vec![Box::new(PassEvaluator)], 1024 * 1024, None);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(bytes.starts_with(&chunk));
	}

	#[tokio::test]
	async fn test_block() {
		let chunk = delta_bytes("bad content");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk, done]);

		let guarded = GuardedSseBody::new(body, vec![Box::new(BlockEvaluator)], 1024 * 1024, None);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"bad content"));
	}

	fn pattern_evaluator(pattern: &str) -> PatternEvaluator {
		PatternEvaluator {
			pattern: regex::Regex::new(pattern).unwrap(),
		}
	}

	#[tokio::test]
	async fn test_regex_blocks_matching_response() {
		let chunk = delta_bytes("my SSN is 123-45-6789");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk, done]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("SSN"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"SSN"));
	}

	fn gemini_delta_bytes(text: &str, thought: &str) -> Bytes {
		sse_bytes(&format!(
			"{{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{}\",\"thought\":true}},{{\"text\":\"{}\"}}]}}}}]}}",
			thought, text
		))
	}

	#[tokio::test]
	async fn test_gemini_sse_text_is_evaluated() {
		let chunk1 = gemini_delta_bytes("my SSN", "planning the answer");
		let chunk2 = gemini_delta_bytes(" is 123-45-6789", "still planning");
		let body = make_body(vec![chunk1, chunk2]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("SSN is"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"123-45-6789"));
	}

	fn gemini_candidates_bytes(candidates: serde_json::Value) -> Bytes {
		sse_bytes(&serde_json::json!({ "candidates": candidates }).to_string())
	}

	#[tokio::test]
	async fn test_gemini_sse_evaluates_every_candidate() {
		// candidateCount is client-controlled: the guard-relevant text sits in candidates[1]
		// behind a candidates[0] that only carries a thought part.
		let chunk = gemini_candidates_bytes(serde_json::json!([
			{ "content": { "role": "model", "parts": [{ "text": "planning", "thought": true }] } },
			{ "content": { "role": "model", "parts": [{ "text": "my SSN is 123-45-6789" }] } }
		]));
		let body = make_body(vec![chunk]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("SSN"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"123-45-6789"));
	}

	#[tokio::test]
	async fn test_gemini_sse_scans_past_a_candidate_without_content() {
		// A candidate can carry only a finishReason; that must not end the scan.
		let chunk = gemini_candidates_bytes(serde_json::json!([
			{ "finishReason": "STOP" },
			{ "content": { "role": "model", "parts": [{ "text": "forbidden words" }] } }
		]));
		let body = make_body(vec![chunk]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("forbidden"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"forbidden"));
	}

	fn text_delta(chunk: serde_json::Value) -> Option<String> {
		GuardedSseBody::extract_text_delta(SseFrame::Event(Event {
			id: None,
			name: "message".into(),
			data: Bytes::from(chunk.to_string()),
		}))
	}

	#[test]
	fn test_extract_text_delta_matches_one_arm_per_chunk_shape() {
		assert_eq!(
			text_delta(serde_json::json!({ "type": "response.output_text.delta", "delta": "a" })),
			Some("a".to_string())
		);
		assert_eq!(
			text_delta(serde_json::json!({ "choices": [{ "delta": { "content": "b" } }] })),
			Some("b".to_string())
		);
		assert_eq!(
			text_delta(serde_json::json!({ "type": "content_block_delta", "delta": { "text": "c" } })),
			Some("c".to_string())
		);
		assert_eq!(
			text_delta(serde_json::json!({
				"candidates": [
					{ "content": { "parts": [{ "text": "d" }] } },
					{ "content": { "parts": [{ "text": "e" }, { "text": "skip", "thought": true }] } }
				]
			})),
			Some("de".to_string())
		);
		assert_eq!(text_delta(serde_json::json!({ "usageMetadata": {} })), None);
	}

	#[tokio::test]
	async fn test_gemini_sse_ignores_thought_parts() {
		let chunk = gemini_delta_bytes("all good", "forbidden");
		let body = make_body(vec![chunk.clone()]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("forbidden"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(bytes.starts_with(&chunk));
		assert!(!contains(&bytes, b"guardrail_blocked"));
	}

	#[tokio::test]
	async fn test_regex_passes_non_matching_response() {
		let chunk = delta_bytes("hello world");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk.clone(), done]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("SSN"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(bytes.starts_with(&chunk));
		assert!(!contains(&bytes, b"guardrail_blocked"));
	}

	#[tokio::test]
	async fn test_regex_accumulates_within_batch() {
		let chunk1 = delta_bytes("my credit");
		let chunk2 = delta_bytes(" card");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk1, chunk2, done]);

		let guarded = GuardedSseBody::new(
			body,
			vec![Box::new(pattern_evaluator("credit card"))],
			1024 * 1024,
			None,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"credit"));
	}

	#[tokio::test]
	async fn test_windowed_incremental_flush_then_block() {
		let chunk1 = delta_bytes("this part is fine");
		let chunk2 = delta_bytes("forbidden words here");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk1.clone(), chunk2, done]);

		let guarded = GuardedSseBody::with_threshold(
			body,
			vec![Box::new(pattern_evaluator("forbidden"))],
			1024 * 1024,
			None,
			4,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"this part is fine"));
		assert!(!contains(&bytes, b"forbidden"));
		assert!(contains(&bytes, b"guardrail_blocked"));
	}

	#[tokio::test]
	async fn test_overlap_catches_boundary_spanning_pattern() {
		let chunk1 = delta_bytes("my credit");
		let chunk2 = delta_bytes(" card number");
		let done = sse_bytes("[DONE]");
		let body = make_body(vec![chunk1, chunk2, done]);

		let guarded = GuardedSseBody::with_threshold(
			body,
			vec![Box::new(pattern_evaluator("credit card"))],
			1024 * 1024,
			None,
			4,
		);

		let bytes = guarded.collect().await.unwrap().to_bytes();
		assert!(contains(&bytes, b"guardrail_blocked"));
		assert!(!contains(&bytes, b"card number"));
	}

	#[test]
	fn test_tail_chars_respects_utf8_boundaries() {
		let s = "héllo wörld";
		let t = tail_chars(s, 4);
		assert!(t.len() <= 4);
		assert!(s.ends_with(t));
		let s2 = "aé";
		assert_eq!(tail_chars(s2, 1), "");
		assert_eq!(tail_chars(s2, 2), "é");
	}

	#[tokio::test]
	async fn evaluate_window_fail_closed_blocks_on_error() {
		use crate::llm::policy::FailureMode;
		let mut evs: Vec<Box<dyn StreamingEvaluator>> = vec![Box::new(ErrorEvaluator {
			mode: FailureMode::FailClosed,
		})];
		assert_eq!(
			evaluate_window(&mut evs, "some text").await.as_deref(),
			Some(&b"Content blocked by guardrail policy"[..])
		);
	}

	#[tokio::test]
	async fn evaluate_window_fail_open_passes_on_error() {
		use crate::llm::policy::FailureMode;
		let mut evs: Vec<Box<dyn StreamingEvaluator>> = vec![Box::new(ErrorEvaluator {
			mode: FailureMode::FailOpen,
		})];
		assert!(evaluate_window(&mut evs, "some text").await.is_none());
	}
}
