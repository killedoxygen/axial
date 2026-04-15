#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Number,
    Word,
    Separator(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionToken {
    pub kind: TokenKind,
    pub raw: String,
    pub normalized: String,
}

pub(crate) fn tokenize_version_id(value: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_kind: Option<TokenKind> = None;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            let next_kind = if ch.is_ascii_digit() {
                TokenKind::Number
            } else {
                TokenKind::Word
            };

            if current_kind
                .as_ref()
                .is_some_and(|kind| !same_group(kind, &next_kind))
                && !current.is_empty()
            {
                tokens.push(build_token(
                    current_kind.take().expect("token kind"),
                    &current,
                ));
                current.clear();
            }

            current.push(ch);
            current_kind = Some(next_kind);
            continue;
        }

        if let Some(kind) = current_kind.take()
            && !current.is_empty()
        {
            tokens.push(build_token(kind, &current));
            current.clear();
        }

        tokens.push(VersionToken {
            kind: TokenKind::Separator(ch),
            raw: ch.to_string(),
            normalized: ch.to_string(),
        });
    }

    if let Some(kind) = current_kind
        && !current.is_empty()
    {
        tokens.push(build_token(kind, &current));
    }

    tokens
}

fn same_group(left: &TokenKind, right: &TokenKind) -> bool {
    matches!(
        (left, right),
        (TokenKind::Number, TokenKind::Number) | (TokenKind::Word, TokenKind::Word)
    )
}

fn build_token(kind: TokenKind, value: &str) -> VersionToken {
    VersionToken {
        normalized: match kind {
            TokenKind::Number => value.to_string(),
            TokenKind::Word => value.to_ascii_lowercase(),
            TokenKind::Separator(_) => value.to_string(),
        },
        raw: value.to_string(),
        kind,
    }
}
