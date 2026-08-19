/// Remove HTTP(S) endpoint strings from diagnostics before they are logged or
/// returned. RPC URLs commonly carry API keys in their path or query.
pub fn urls(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = next_url(rest) {
        output.push_str(&rest[..start]);
        output.push_str("<RPC URL>");
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find_map(|(index, character)| (index > 0 && is_terminator(character)).then_some(index))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    output.push_str(rest);
    output
}

pub fn error(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(urls(&format!("{error:#}")))
}

fn next_url(message: &str) -> Option<usize> {
    [message.find("https://"), message.find("http://")]
        .into_iter()
        .flatten()
        .min()
}

fn is_terminator(character: char) -> bool {
    character.is_whitespace() || matches!(character, ')' | ']' | '}' | '>' | '"' | '\'' | ',' | ';')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_credentials_are_removed_from_diagnostics() {
        let message =
            "request to https://rpc.example/v2/private-key?x=secret failed (http://fallback/key)";
        let redacted = urls(message);
        assert_eq!(redacted, "request to <RPC URL> failed (<RPC URL>)");
        assert!(!redacted.contains("private-key"));
        assert!(!redacted.contains("secret"));
    }
}
