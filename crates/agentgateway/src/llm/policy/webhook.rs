use ::http::header::CONTENT_TYPE;
use ::http::{HeaderMap, HeaderValue, header};
pub use agent_llm::webhook::{Message, ResponseChoice};
use serde::{Deserialize, Serialize};

use crate::cel::RequestSnapshot;
use crate::http::{HeaderOrPseudoValue, RequestOrResponse};
use crate::llm::policy::{Webhook, with_default_timeout};
use crate::proxy::httpproxy::PolicyClient;
use crate::telemetry::metrics::{OutboundCallKind, OutboundCallSubtype};
use crate::*;

const REQUEST_PATH: &str = "request";
const RESPONSE_PATH: &str = "response";

#[derive(Clone, Copy)]
pub(super) struct EvaluationContext<'a> {
	request: Option<&'a RequestSnapshot>,
	llm_request: Option<&'a serde_json::Value>,
}

impl<'a> EvaluationContext<'a> {
	pub(super) fn new(
		request: Option<&'a RequestSnapshot>,
		llm_request: Option<&'a serde_json::Value>,
	) -> Self {
		Self {
			request,
			llm_request,
		}
	}

	fn executor(self) -> cel::Executor<'a> {
		match self.llm_request {
			Some(llm_request) => cel::Executor::new_llm(self.request, llm_request),
			None => cel::Executor::new_request_snapshot(self.request),
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GuardrailsPromptRequest {
	/// body contains the object which is a list of the Message JSON objects from the prompts in the request
	pub body: PromptMessages,
	/// request is the parsed request body in the client's format; only with `includeBody`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub request: Option<serde_json::Value>,
	/// normalized_messages is every message with its text, tool calls, tool results and reasoning; only with `includeBody`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub normalized_messages: Option<serde_json::Value>,
}

/// The `includeBody` additions to a request payload.
#[derive(Debug, Clone, Default)]
pub(super) struct RequestExtras {
	pub request: Option<serde_json::Value>,
	pub normalized_messages: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GuardrailsPromptResponse {
	/// action is the action to be taken based on the request.
	/// The following actions are available on the response:
	/// - PassAction: No action is required.
	/// - MaskAction: Mask the response body.
	/// - RejectAction: Reject the request.
	pub action: RequestAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GuardrailsResponseRequest {
	/// body contains the object with a list of Choice that contains the response content from the LLM.
	pub body: ResponseChoices,
	/// response is the parsed response in the client's format; only with `includeBody`, never for a streamed window.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub response: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GuardrailsResponseResponse {
	/// action is the action to be taken based on the request.
	/// The following actions are available on the response:
	/// - PassAction: No action is required.
	/// - MaskAction: Mask the response body.
	/// - RejectAction: Reject the response.
	pub action: ResponseAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptMessages {
	/// List of prompt messages including role and content.
	pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResponseChoices {
	/// list of possible independent responses from the LLM
	pub choices: Vec<ResponseChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PassAction {
	/// reason is a human readable string that explains the reason for the action.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MaskAction {
	/// body contains the modified messages that masked out some of the original contents.
	/// When used in a GuardrailPromptResponse, this should be PromptMessages.
	/// When used in GuardrailResponseResponse, this should be ResponseChoices
	pub body: MaskActionBody,
	/// reason is a human readable string that explains the reason for the action.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RejectAction {
	/// body is the rejection message that will be used for HTTP error response body.
	pub body: String,
	/// status_code is the HTTP status code to be returned in the HTTP error response.
	pub status_code: u16,
	/// reason is a human readable string that explains the reason for the action.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reason: Option<String>,
}

/// Enum for actions available in prompt responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, rename_all = "snake_case")]
pub enum RequestAction {
	Mask(MaskAction),
	Reject(RejectAction),
	Pass(PassAction),
}

/// Enum for actions available in response responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, rename_all = "snake_case")]
pub enum ResponseAction {
	Mask(MaskAction),
	Reject(RejectAction),
	Pass(PassAction),
}

/// Enum for MaskAction body that can be either PromptMessages or ResponseChoices
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaskActionBody {
	PromptMessages(PromptMessages),
	ResponseChoices(ResponseChoices),
}

/// Keys whose string value is a data URI (`data:<media type>;base64,<payload>`): OpenAI chat
/// `image_url.url` and `file.file_data`, Responses `image_url`.
const DATA_URI_KEYS: [&str; 3] = ["url", "image_url", "file_data"];
/// Sibling keys that mark a bare base64 `data` string and name its media type: Anthropic
/// `source`, Gemini `inlineData` / `inline_data`.
const MEDIA_TYPE_KEYS: [&str; 3] = ["media_type", "mime_type", "mimeType"];

/// Replace every inline binary payload in a parsed request or response with
/// `omitted:<media type>;bytes=<decoded length>;sha256=<hex of the decoded bytes>`. Covers base64
/// data URIs under the [`DATA_URI_KEYS`], a `data` string beside one of the [`MEDIA_TYPE_KEYS`] or
/// beside an audio `format` (OpenAI `input_audio`), and `b64_json` (generated images). A string
/// that does not decode as base64 is left as it is, as is every other string.
pub(crate) fn describe_binary_parts(value: &mut serde_json::Value) {
	match value {
		serde_json::Value::Array(items) => items.iter_mut().for_each(describe_binary_parts),
		serde_json::Value::Object(fields) => {
			let media_type = fields
				.iter()
				.find(|(key, _)| MEDIA_TYPE_KEYS.contains(&key.as_str()))
				.and_then(|(_, value)| value.as_str().map(str::to_string))
				.or_else(|| {
					fields
						.get("format")
						.and_then(serde_json::Value::as_str)
						.map(|format| format!("audio/{format}"))
				});
			for (key, field) in fields.iter_mut() {
				let Some(text) = field.as_str() else {
					describe_binary_parts(field);
					continue;
				};
				let descriptor = if DATA_URI_KEYS.contains(&key.as_str()) {
					describe_data_uri(text)
				} else if key == "b64_json" {
					describe_base64("application/octet-stream", text)
				} else if key == "data" {
					media_type
						.as_deref()
						.and_then(|media_type| describe_base64(media_type, text))
				} else {
					None
				};
				if let Some(descriptor) = descriptor {
					*field = serde_json::Value::String(descriptor);
				}
			}
		},
		_ => {},
	}
}

fn describe_data_uri(text: &str) -> Option<String> {
	let (header, payload) = text.strip_prefix("data:")?.split_once(',')?;
	let mut parameters = header.split(';');
	let media_type = parameters
		.next()
		.filter(|m| !m.is_empty())
		.unwrap_or("text/plain");
	parameters
		.any(|parameter| parameter == "base64")
		.then(|| describe_base64(media_type, payload))
		.flatten()
}

fn describe_base64(media_type: &str, text: &str) -> Option<String> {
	use base64::Engine;
	use base64::prelude::{BASE64_STANDARD, BASE64_STANDARD_NO_PAD};
	let bytes = BASE64_STANDARD
		.decode(text)
		.or_else(|_| BASE64_STANDARD_NO_PAD.decode(text))
		.ok()?;
	let digest = hex::encode(crate::crypto::digest::sha256(&bytes));
	Some(format!(
		"omitted:{media_type};bytes={};sha256={digest}",
		bytes.len()
	))
}

fn build_request_for_request(
	webhook: &Webhook,
	context: EvaluationContext<'_>,
	http_headers: &HeaderMap,
	messages: Vec<Message>,
	extras: RequestExtras,
) -> anyhow::Result<crate::http::Request> {
	let body = GuardrailsPromptRequest {
		body: PromptMessages { messages },
		request: extras.request,
		normalized_messages: extras.normalized_messages,
	};
	build_request(&body, REQUEST_PATH, webhook, context, http_headers)
}

fn build_request_for_response(
	webhook: &Webhook,
	context: EvaluationContext<'_>,
	http_headers: &HeaderMap,
	choices: Vec<ResponseChoice>,
	response: Option<serde_json::Value>,
) -> anyhow::Result<crate::http::Request> {
	let body = GuardrailsResponseRequest {
		body: ResponseChoices { choices },
		response,
	};
	build_request(&body, RESPONSE_PATH, webhook, context, http_headers)
}

fn build_request<T: serde::Serialize>(
	body: &T,
	path: &str,
	webhook: &Webhook,
	context: EvaluationContext<'_>,
	http_headers: &HeaderMap,
) -> anyhow::Result<crate::http::Request> {
	let body_bytes = serde_json::to_vec(body)?;
	let mut rb = ::http::Request::builder()
		.uri(format!("/{path}"))
		.method(http::Method::POST);
	for (k, v) in http_headers {
		// TODO: this is configurable by users
		if k == header::CONTENT_LENGTH {
			// TODO: probably others
			continue;
		}
		rb = rb.header(k.clone(), v.clone());
	}
	let mut req = rb
		.header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
		.body(crate::http::Body::from(body_bytes))?;
	apply_header_expressions(&mut req, webhook, context);
	Ok(req)
}

/// Apply the configured CEL header expressions to the outgoing webhook request.
/// Setting the `:path` pseudo-header replaces the default `/request` / `/response` path.
///
/// Expressions are evaluated against the original incoming request (like the
/// `transformation` and `extAuthz` policies), so `request.*`, `jwt.*`, etc. refer to
/// the client's request, not the webhook request being built.
fn apply_header_expressions(
	req: &mut crate::http::Request,
	webhook: &Webhook,
	context: EvaluationContext<'_>,
) {
	if webhook.headers.is_empty() {
		return;
	}
	let exec = context.executor();
	for (k, expr) in &webhook.headers {
		if matches!(
			k,
			crate::http::HeaderOrPseudo::Scheme | crate::http::HeaderOrPseudo::Status
		) {
			debug!("unsupported webhook pseudo-header {k}; skipping");
			continue;
		}
		let v = match exec.eval(expr) {
			Ok(v) => Some(v),
			Err(e) => {
				debug!("webhook header expression for {k} failed: {e}");
				None
			},
		};
		let val = HeaderOrPseudoValue::from_cel_result(k, v);
		if val.is_none() {
			debug!("webhook header expression for {k} did not produce a value");
		}
		RequestOrResponse::Request(req).apply_header(
			k,
			val,
			http::HeaderMutationAction::OverwriteIfExistsOrAdd,
		);
	}
}

pub(super) async fn send_request(
	client: &PolicyClient,
	webhook: &Webhook,
	context: EvaluationContext<'_>,
	http_headers: &HeaderMap,
	messages: Vec<Message>,
	extras: RequestExtras,
) -> anyhow::Result<GuardrailsPromptResponse> {
	let whr = with_default_timeout(build_request_for_request(
		webhook,
		context,
		http_headers,
		messages,
		extras,
	)?);
	let res = Box::pin(
		client
			.with_outbound(OutboundCallKind::Policy, OutboundCallSubtype::Guardrail)
			.call_reference_with_policies(
				whr,
				&webhook.target.target,
				webhook.target.policies.as_slice(),
			),
	)
	.await?;
	let parsed = json::from_response_body(res).await?;
	Ok(parsed)
}

pub(super) async fn send_response(
	client: &PolicyClient,
	webhook: &Webhook,
	context: EvaluationContext<'_>,
	http_headers: &HeaderMap,
	choices: Vec<ResponseChoice>,
	response: Option<serde_json::Value>,
) -> anyhow::Result<GuardrailsResponseResponse> {
	let whr = with_default_timeout(build_request_for_response(
		webhook,
		context,
		http_headers,
		choices,
		response,
	)?);
	let res = client
		.with_outbound(OutboundCallKind::Policy, OutboundCallSubtype::Guardrail)
		.call_reference_with_policies(
			whr,
			&webhook.target.target,
			webhook.target.policies.as_slice(),
		)
		.await?;
	let parsed = json::from_response_body(res).await?;
	Ok(parsed)
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use secrecy::SecretString;

	use super::*;
	use crate::http::HeaderOrPseudo;
	use crate::http::jwt::Claims;
	use crate::llm::policy::{BinaryParts, FailureMode, RejectAuditAction};
	use crate::types::agent::{SimpleBackendReference, SimpleBackendReferenceWithPolicies};

	fn webhook(headers: Vec<(HeaderOrPseudo, Arc<cel::Expression>)>) -> Webhook {
		Webhook {
			target: SimpleBackendReferenceWithPolicies {
				target: Arc::new(SimpleBackendReference::Invalid),
				policies: vec![],
			},
			headers,
			forward_header_matches: vec![],
			failure_mode: FailureMode::FailClosed,
			action: RejectAuditAction::Reject,
			include_body: false,
			binary_parts: BinaryParts::Inline,
		}
	}

	fn expr(s: &str) -> Arc<cel::Expression> {
		Arc::new(cel::Expression::new_strict(s).unwrap())
	}

	/// Snapshot of an original client request: POST /v1/chat/completions with an
	/// x-tenant header and (optionally) JWT claims.
	fn original(claims: Option<Claims>) -> RequestSnapshot {
		let mut req = ::http::Request::builder()
			.method(http::Method::POST)
			.uri("http://example.com/v1/chat/completions")
			.header("x-tenant", "acme")
			.body(crate::http::Body::empty())
			.unwrap();
		if let Some(claims) = claims {
			req.extensions_mut().insert(claims);
		}
		cel::snapshot_request(&mut req, false)
	}

	fn claims() -> Claims {
		Claims {
			inner: serde_json::from_value(serde_json::json!({"sub": "user-123"})).unwrap(),
			jwt: SecretString::from("token"),
		}
	}

	#[test]
	fn default_paths_are_preserved() {
		let wh = webhook(vec![]);
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		assert_eq!(req.uri().path(), "/request");
		let resp = build_request_for_response(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			None,
		)
		.unwrap();
		assert_eq!(resp.uri().path(), "/response");
	}

	#[test]
	fn path_pseudo_header_overrides_default_path() {
		let wh = webhook(Vec::from([(
			HeaderOrPseudo::Path,
			expr(r#""/v3/guardrails/agentgateway/request""#),
		)]));
		let snap = original(None);
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(Some(&snap), None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		assert_eq!(req.uri().path(), "/v3/guardrails/agentgateway/request");
	}

	#[test]
	fn expressions_see_the_original_request() {
		// request.* refers to the original client request, not the webhook request.
		let wh = webhook(Vec::from([
			(
				HeaderOrPseudo::Header(::http::HeaderName::from_static("x-orig-path")),
				expr("request.path"),
			),
			(
				HeaderOrPseudo::Header(::http::HeaderName::from_static("x-tenant")),
				expr(r#"request.headers["x-tenant"]"#),
			),
			(
				HeaderOrPseudo::Header(::http::HeaderName::from_static("x-model")),
				expr("llmRequest.model"),
			),
		]));
		let snap = original(None);
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(Some(&snap), Some(&serde_json::json!({"model": "llama3.2"}))),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		assert_eq!(
			req.headers().get("x-orig-path").unwrap(),
			"/v1/chat/completions"
		);
		assert_eq!(req.headers().get("x-tenant").unwrap(), "acme");
		assert_eq!(req.headers().get("x-model").unwrap(), "llama3.2");
		// The webhook path itself is untouched by non-:path expressions.
		assert_eq!(req.uri().path(), "/request");
	}

	#[test]
	fn jwt_claims_available_in_expressions() {
		let wh = webhook(Vec::from([(
			HeaderOrPseudo::Header(::http::HeaderName::from_static("x-user")),
			expr("jwt.sub"),
		)]));
		let snap = original(Some(claims()));
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(Some(&snap), None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		assert_eq!(req.headers().get("x-user").unwrap(), "user-123");
	}

	#[test]
	fn claims_are_not_attached_to_the_outgoing_request() {
		let wh = webhook(Vec::from([(
			HeaderOrPseudo::Header(::http::HeaderName::from_static("x-user")),
			expr("jwt.sub"),
		)]));
		let snap = original(Some(claims()));
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(Some(&snap), None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		// Claims are only exposed to CEL evaluation; leaking them onto the request
		// would hand the raw JWT to the webhook backend's policy chain (e.g.
		// backendAuth passthrough).
		assert!(req.extensions().get::<Claims>().is_none());
	}

	#[test]
	fn failed_expression_removes_header_and_keeps_path() {
		// An expression referencing missing context must not set the header,
		// and a failed :path expression must leave the default path intact.
		let wh = webhook(Vec::from([
			(
				HeaderOrPseudo::Header(::http::HeaderName::from_static("x-missing")),
				expr(r#"request.headers["does-not-exist"]"#),
			),
			(HeaderOrPseudo::Path, expr("jwt.missing_claim")),
		]));
		let snap = original(None);
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(Some(&snap), None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		assert!(req.headers().get("x-missing").is_none());
		assert_eq!(req.uri().path(), "/request");
	}

	#[test]
	fn no_snapshot_degrades_gracefully() {
		// With no original-request snapshot (e.g. context unavailable), expressions
		// fail soft: default path kept, headers unset.
		let wh = webhook(Vec::from([
			(HeaderOrPseudo::Path, expr(r#""/prefixed/request""#)),
			(
				HeaderOrPseudo::Header(::http::HeaderName::from_static("x-user")),
				expr("jwt.sub"),
			),
		]));
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.unwrap();
		// Static expressions still work without a snapshot...
		assert_eq!(req.uri().path(), "/prefixed/request");
		// ...but context-dependent ones are skipped.
		assert!(req.headers().get("x-user").is_none());
	}

	/// `target` accepts the same `policies` slot extAuthz has: explicit
	/// `backendTLS`, or an `https://` host that implies it. The historical
	/// `{host}` form yields no policies, so existing configs are unchanged.
	#[test]
	fn target_policies_deserialize_and_https_scheme_implies_tls() {
		use crate::types::agent::{BackendTrafficPolicy, Target};

		let plain: Webhook = serde_json::from_value(serde_json::json!({
			"target": {"host": "127.0.0.1:8000"}
		}))
		.unwrap();
		assert!(plain.target.policies.is_empty());
		let serialized = serde_json::to_value(&plain).unwrap();
		assert!(serialized["target"].get("policies").is_none());

		let explicit: Webhook = serde_json::from_value(serde_json::json!({
			"target": {"host": "guard.example.com:8443", "policies": {"backendTLS": {}}}
		}))
		.unwrap();
		assert!(matches!(
			explicit.target.policies.as_slice(),
			[BackendTrafficPolicy::BackendTLS(_)]
		));

		let scheme: Webhook = serde_json::from_value(serde_json::json!({
			"target": {"host": "https://guard.example.com"}
		}))
		.unwrap();
		assert!(matches!(
			scheme.target.target.as_ref(),
			SimpleBackendReference::InlineBackend(Target::Hostname(host, 443))
				if host.as_str() == "guard.example.com"
		));
		assert!(matches!(
			scheme.target.policies.as_slice(),
			[BackendTrafficPolicy::BackendTLS(_)]
		));
	}

	/// The policies are honored on the wire: an https webhook is reached with
	/// the target's `backendTLS`, and the same target without it (plaintext to
	/// a TLS listener) fails.
	#[cfg(feature = "crypto-aws-lc")]
	#[tokio::test]
	async fn https_target_is_dialed_with_backend_tls() {
		use wiremock::matchers::{method, path};
		use wiremock::{Mock, MockServer, ResponseTemplate};

		use crate::http::backendtls::{BackendTLS, ResolvedBackendTLS};
		use crate::transport::tls;
		use crate::types::agent::{BackendTrafficPolicy, Target};

		let _ = rustls::crypto::CryptoProvider::install_default(Arc::unwrap_or_clone(tls::provider()));
		let certs = wiremock::tls_certs::MockTlsCertificates::random();
		let server = MockServer::builder()
			.start_https(certs.get_server_config())
			.await;
		Mock::given(method("POST"))
			.and(path("/request"))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"action": {"body": "blocked by guard", "status_code": 403}
			})))
			.mount(&server)
			.await;
		let target = Arc::new(SimpleBackendReference::InlineBackend(Target::Address(
			*server.address(),
		)));
		let tls: BackendTLS = ResolvedBackendTLS {
			root: Some(certs.root_cert.pem().into_bytes()),
			insecure_host: true,
			..Default::default()
		}
		.try_into()
		.unwrap();
		let client = crate::test_helpers::policy_client();

		let mut wh = webhook(vec![]);
		wh.target = SimpleBackendReferenceWithPolicies {
			target: target.clone(),
			policies: vec![BackendTrafficPolicy::BackendTLS(tls)],
		};
		let verdict = send_request(
			&client,
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.await
		.expect("https webhook reachable with backendTLS");
		assert!(matches!(
			verdict.action,
			RequestAction::Reject(RejectAction { ref body, status_code: 403, .. })
				if body == "blocked by guard"
		));

		wh.target = SimpleBackendReferenceWithPolicies {
			target,
			policies: vec![],
		};
		let plaintext = send_request(
			&client,
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			RequestExtras::default(),
		)
		.await;
		assert!(
			plaintext.is_err(),
			"plaintext dial to a TLS webhook must fail"
		);
	}

	async fn body_json(req: crate::http::Request) -> serde_json::Value {
		use http_body_util::BodyExt as _;
		let bytes = req.into_body().collect().await.unwrap().to_bytes();
		serde_json::from_slice(&bytes).unwrap()
	}

	/// Without `includeBody` the payload is exactly what it was before the field existed.
	#[tokio::test]
	async fn payload_is_unchanged_without_include_body() {
		let wh = webhook(vec![]);
		let messages = vec![Message {
			role: "user".into(),
			content: "hi".into(),
		}];
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			messages,
			RequestExtras::default(),
		)
		.unwrap();
		assert_eq!(
			body_json(req).await,
			serde_json::json!({"body": {"messages": [{"role": "user", "content": "hi"}]}})
		);
		let choices = vec![ResponseChoice {
			message: Message {
				role: "assistant".into(),
				content: "yo".into(),
			},
		}];
		let resp = build_request_for_response(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			choices,
			None,
		)
		.unwrap();
		assert_eq!(
			body_json(resp).await,
			serde_json::json!({"body": {"choices": [{"message": {"role": "assistant", "content": "yo"}}]}})
		);
	}

	#[tokio::test]
	async fn include_body_adds_request_response_and_normalized_messages() {
		let wh = webhook(vec![]);
		let extras = RequestExtras {
			request: Some(serde_json::json!({"model": "m", "tools": [{"type": "function"}]})),
			normalized_messages: Some(serde_json::json!([{"role": "user", "parts": []}])),
		};
		let req = build_request_for_request(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			extras,
		)
		.unwrap();
		let body = body_json(req).await;
		assert_eq!(body["request"]["tools"][0]["type"], "function");
		assert_eq!(body["normalized_messages"][0]["role"], "user");
		assert_eq!(body["body"]["messages"], serde_json::json!([]));

		let resp = build_request_for_response(
			&wh,
			EvaluationContext::new(None, None),
			&HeaderMap::new(),
			vec![],
			Some(serde_json::json!({"usage": {"total_tokens": 3}})),
		)
		.unwrap();
		assert_eq!(
			body_json(resp).await["response"]["usage"]["total_tokens"],
			3
		);
	}

	/// With `includeBody` the webhook sees the tools, the tool call and the tool result the
	/// message list cannot carry; without it the payload is the message list alone.
	#[tokio::test]
	async fn include_body_sends_tools_and_tool_calls_through_the_request_guard() {
		use wiremock::matchers::{method, path};
		use wiremock::{Mock, MockServer, ResponseTemplate};

		use crate::types::agent::Target;

		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(path("/request"))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(serde_json::json!({"action": {"reason": "ok"}})),
			)
			.mount(&server)
			.await;
		let client = crate::test_helpers::policy_client();
		let request = serde_json::json!({
			"model": "gpt-4o",
			"tools": [{"type": "function", "function": {"name": "search_patients", "parameters": {"type": "object"}}}],
			"tool_choice": "auto",
			"messages": [
				{"role": "system", "content": "be brief"},
				{"role": "user", "content": "find Jane"},
				{"role": "assistant", "content": null, "tool_calls": [
					{"id": "call_1", "type": "function", "function": {"name": "search_patients", "arguments": "{\"name\":\"Jane\"}"}}
				]},
				{"role": "tool", "tool_call_id": "call_1", "content": "mrn 123"}
			]
		});
		for include_body in [false, true] {
			let mut wh = webhook(vec![]);
			wh.target = SimpleBackendReferenceWithPolicies {
				target: Arc::new(SimpleBackendReference::InlineBackend(Target::Address(
					*server.address(),
				))),
				policies: vec![],
			};
			wh.include_body = include_body;
			let mut req: crate::llm::types::completions::Request =
				serde_json::from_value(request.clone()).unwrap();
			let (outcome, _) = super::super::Policy::evaluate_webhook_request(
				&mut req,
				&HeaderMap::new(),
				&client,
				&wh,
				None,
			)
			.await
			.expect("webhook reachable");
			assert!(matches!(outcome, super::super::GuardrailOutcome::None));
		}

		let received = server.received_requests().await.unwrap();
		assert_eq!(received.len(), 2);
		let plain: serde_json::Value = received[0].body_json().unwrap();
		assert!(plain.get("request").is_none());
		assert!(plain.get("normalized_messages").is_none());
		assert_eq!(plain["body"]["messages"].as_array().unwrap().len(), 4);

		let full: serde_json::Value = received[1].body_json().unwrap();
		assert_eq!(full["body"], plain["body"]);
		assert_eq!(
			full["request"]["tools"][0]["function"]["name"],
			"search_patients"
		);
		assert_eq!(full["request"]["tool_choice"], "auto");
		assert_eq!(full["request"]["model"], "gpt-4o");
		let part_types: Vec<&str> = full["normalized_messages"]
			.as_array()
			.unwrap()
			.iter()
			.flat_map(|m| m["parts"].as_array().unwrap().iter())
			.map(|p| p["type"].as_str().unwrap())
			.collect();
		assert!(part_types.contains(&"toolCall"), "{part_types:?}");
		assert!(part_types.contains(&"toolResult"), "{part_types:?}");
	}

	/// Every inline binary shape the engine's formats carry becomes a descriptor; URLs, text and
	/// strings that are not base64 stay as they are. sha256("hi") is the digest below.
	#[test]
	fn binary_parts_become_descriptors() {
		let hi = "omitted:image/png;bytes=2;sha256=8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
		let mut value = serde_json::json!({
			"messages": [{"role": "user", "content": [
				{"type": "text", "text": "data:image/png;base64,aGk= is a string, not a part"},
				{"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}},
				{"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
				{"type": "input_audio", "input_audio": {"data": "aGk=", "format": "wav"}},
				{"type": "file", "file": {"file_data": "data:application/pdf;base64,aGk=", "filename": "a.pdf"}}
			]}],
			"input": [{"type": "input_image", "image_url": "data:image/png;base64,aGk"}],
			"anthropic": {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
			"gemini": {"inlineData": {"mimeType": "image/png", "data": "aGk="}},
			"generated": {"b64_json": "aGk="},
			"data": "not binary: no media type beside it",
			"url": "data:text/plain,hello"
		});
		describe_binary_parts(&mut value);
		let parts = &value["messages"][0]["content"];
		assert!(
			parts[0]["text"]
				.as_str()
				.unwrap()
				.starts_with("data:image/png;base64,aGk=")
		);
		assert_eq!(parts[1]["image_url"]["url"], hi);
		assert_eq!(parts[2]["image_url"]["url"], "https://example.com/a.png");
		assert_eq!(
			parts[3]["input_audio"]["data"],
			hi.replace("image/png", "audio/wav")
		);
		assert_eq!(
			parts[4]["file"]["file_data"],
			hi.replace("image/png", "application/pdf")
		);
		assert_eq!(parts[4]["file"]["filename"], "a.pdf");
		assert_eq!(value["input"][0]["image_url"], hi);
		assert_eq!(value["anthropic"]["source"]["data"], hi);
		assert_eq!(value["anthropic"]["source"]["media_type"], "image/png");
		assert_eq!(value["gemini"]["inlineData"]["data"], hi);
		assert_eq!(
			value["generated"]["b64_json"],
			hi.replace("image/png", "application/octet-stream")
		);
		assert_eq!(value["data"], "not binary: no media type beside it");
		assert_eq!(value["url"], "data:text/plain,hello");
	}

	/// With `binaryParts: descriptor` the request guard receives the descriptor in `request`
	/// while the engine's own copy of the request keeps the image for the upstream.
	#[tokio::test]
	async fn descriptor_binary_parts_shrink_the_request_payload_only() {
		use wiremock::matchers::{method, path};
		use wiremock::{Mock, MockServer, ResponseTemplate};

		use crate::types::agent::Target;

		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(path("/request"))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(serde_json::json!({"action": {"reason": "ok"}})),
			)
			.mount(&server)
			.await;
		let client = crate::test_helpers::policy_client();
		let mut wh = webhook(vec![]);
		wh.target = SimpleBackendReferenceWithPolicies {
			target: Arc::new(SimpleBackendReference::InlineBackend(Target::Address(
				*server.address(),
			))),
			policies: vec![],
		};
		wh.include_body = true;
		wh.binary_parts = BinaryParts::Descriptor;

		let image = "data:image/png;base64,aGk=";
		let mut req: crate::llm::types::completions::Request =
			serde_json::from_value(serde_json::json!({
				"model": "gpt-4o",
				"messages": [{"role": "user", "content": [
					{"type": "text", "text": "what is this"},
					{"type": "image_url", "image_url": {"url": image}}
				]}]
			}))
			.unwrap();
		super::super::Policy::evaluate_webhook_request(&mut req, &HeaderMap::new(), &client, &wh, None)
			.await
			.expect("webhook reachable");

		let received = server.received_requests().await.unwrap();
		let payload: serde_json::Value = received[0].body_json().unwrap();
		let url = payload["request"]["messages"][0]["content"][1]["image_url"]["url"]
			.as_str()
			.unwrap();
		assert!(
			url.starts_with("omitted:image/png;bytes=2;sha256="),
			"{url}"
		);
		let kept = crate::llm::RequestType::to_value(&req).unwrap();
		assert_eq!(kept["messages"][0]["content"][1]["image_url"]["url"], image);
	}

	/// A buffered response carries `response` (tool calls, finish reason, usage); a streamed
	/// window never does.
	#[tokio::test]
	async fn include_body_sends_the_parsed_response_but_not_a_streamed_window() {
		use wiremock::matchers::{method, path};
		use wiremock::{Mock, MockServer, ResponseTemplate};

		use crate::types::agent::Target;

		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(path("/response"))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(serde_json::json!({"action": {"reason": "ok"}})),
			)
			.mount(&server)
			.await;
		let client = crate::test_helpers::policy_client();
		let mut wh = webhook(vec![]);
		wh.target = SimpleBackendReferenceWithPolicies {
			target: Arc::new(SimpleBackendReference::InlineBackend(Target::Address(
				*server.address(),
			))),
			policies: vec![],
		};
		wh.include_body = true;

		let mut resp: crate::llm::types::completions::Response = serde_json::from_value(serde_json::json!({
			"id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-2024-08-06",
			"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
				"role": "assistant", "content": null,
				"tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "search_patients", "arguments": "{}"}}]
			}}],
			"usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
		}))
		.unwrap();
		super::super::Policy::evaluate_webhook_response(
			&mut resp,
			&HeaderMap::new(),
			&client,
			&wh,
			None,
		)
		.await
		.expect("webhook reachable");

		let mut window = super::super::TextResponse {
			content: "a window of text".to_string(),
		};
		super::super::Policy::evaluate_webhook_response(
			&mut window,
			&HeaderMap::new(),
			&client,
			&wh,
			None,
		)
		.await
		.expect("webhook reachable");

		let received = server.received_requests().await.unwrap();
		assert_eq!(received.len(), 2);
		let buffered: serde_json::Value = received[0].body_json().unwrap();
		assert_eq!(buffered["response"]["model"], "gpt-4o-2024-08-06");
		assert_eq!(
			buffered["response"]["choices"][0]["finish_reason"],
			"tool_calls"
		);
		assert_eq!(
			buffered["response"]["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
			"search_patients"
		);
		assert_eq!(buffered["response"]["usage"]["total_tokens"], 15);
		let streamed: serde_json::Value = received[1].body_json().unwrap();
		assert!(streamed.get("response").is_none());
		assert_eq!(
			streamed["body"]["choices"][0]["message"]["content"],
			"a window of text"
		);
	}
}
