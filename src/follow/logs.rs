use alloy::providers::Provider;
use alloy::rpc::types::eth::Log;
use anyhow::Result;

pub async fn fetch_logs<P: Provider>(
    provider: &P,
    from: u64,
    to: u64,
    chunk: &mut u64,
) -> Result<(Vec<Log>, u64)> {
    let mut hi = to;
    loop {
        match provider.get_logs(&crate::chain::filter(from, hi)).await {
            Ok(logs) => return Ok((logs, hi)),
            Err(e) if is_result_cap(&e) && hi > from => narrow_range(from, &mut hi, chunk),
            Err(e) => return Err(e.into()),
        }
    }
}

fn narrow_range(from: u64, hi: &mut u64, chunk: &mut u64) {
    *hi = from + (*hi - from) / 2;
    *chunk = (*chunk / 2).max(1);
    tracing::warn!(
        from,
        hi = *hi,
        chunk = *chunk,
        "result cap hit; narrowing range"
    );
}

pub fn is_result_cap(
    e: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>,
) -> bool {
    is_result_cap_message(&e.to_string())
}

pub fn is_result_cap_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    result_cap_patterns().iter().any(|pat| msg.contains(pat))
}

fn result_cap_patterns() -> [&'static str; 10] {
    [
        "more than",
        "query returned",
        "too many results",
        "response size",
        "limit exceeded",
        "limited to",
        "block range",
        "range is too large",
        "too large",
        "exceeds",
    ]
}
