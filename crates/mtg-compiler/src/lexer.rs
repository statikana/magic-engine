use mtg_ir::{ManaColor, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManaSymbolKind {
    Generic(u32),
    Variable(char),
    Colored(ManaColor),
    Snow,
    Hybrid(ManaColor, ManaColor),
    TwoBrid(ManaColor),
    Phyrexian(ManaColor),
    HybridPhyrexian(ManaColor, ManaColor),
    ColorlessHybrid(ManaColor),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    Comma,
    Period,
    Colon,
    Semicolon,
    EmDash,
    EnDash,
    Hyphen,
    Slash,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Bullet,
    Plus,
    Bang,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedPart {
    Int(i32),
    PlusX,
    MinusX,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind<'a> {
    ManaSymbol(ManaSymbolKind),
    Tap,
    Untap,
    Energy,
    Pawprint,
    Chaos,
    Infinity,

    Number(u32),
    NumberWord(u32),
    X,
    SignedPt {
        power: SignedPart,
        toughness: SignedPart,
    },
    SignedNumber(i32),

    SelfRef,
    QuotedSpan(&'a str),

    Punctuation(Punct),
    Word(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind<'a>,
    pub raw: &'a str,
    pub span: Span,
}

impl<'a> Token<'a> {
    #[inline]
    pub fn eq_ci(&self, expected: &str) -> bool {
        self.raw.eq_ignore_ascii_case(expected)
    }

    #[inline]
    pub fn as_u32(&self) -> Option<u32> {
        match self.kind {
            TokenKind::Number(n) | TokenKind::NumberWord(n) => Some(n),
            _ => None,
        }
    }

    #[inline]
    pub fn is_word(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::Word(_) | TokenKind::NumberWord(_) | TokenKind::X
        )
    }

    #[inline]
    pub fn is_punct(&self, p: Punct) -> bool {
        matches!(self.kind, TokenKind::Punctuation(got) if got == p)
    }
}

/// Zero-allocation slice cursor with backtracking for recursive-descent parsers.
#[derive(Debug, Clone, Copy)]
pub struct TokenCursor<'a> {
    tokens: &'a [Token<'a>],
    pos: usize,
}

impl<'a> TokenCursor<'a> {
    #[inline]
    pub fn new(tokens: &'a [Token<'a>]) -> Self {
        Self { tokens, pos: 0 }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn save(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn restore(&mut self, saved: usize) {
        self.pos = saved;
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    #[inline]
    pub fn remaining(&self) -> &'a [Token<'a>] {
        &self.tokens[self.pos..]
    }

    #[inline]
    pub fn peek(&self) -> Option<&'a Token<'a>> {
        self.tokens.get(self.pos)
    }

    #[inline]
    pub fn peek_n(&self, offset: usize) -> Option<&'a Token<'a>> {
        self.tokens.get(self.pos + offset)
    }

    #[inline]
    pub fn bump(&mut self) -> Option<&'a Token<'a>> {
        let tok = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(tok)
    }

    #[inline]
    pub fn eat_ci(&mut self, word: &str) -> bool {
        if self.peek().is_some_and(|t| t.eq_ci(word)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn eat_punct(&mut self, p: Punct) -> bool {
        if self.peek().is_some_and(|t| t.is_punct(p)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn parse_mana_color(s: &str) -> Option<ManaColor> {
    match s {
        "W" => Some(ManaColor::W),
        "U" => Some(ManaColor::U),
        "B" => Some(ManaColor::B),
        "R" => Some(ManaColor::R),
        "G" => Some(ManaColor::G),
        "C" => Some(ManaColor::C),
        _ => None,
    }
}

fn classify_braced_symbol<'a>(inner: &str) -> TokenKind<'a> {
    if inner.len() <= 16 {
        let mut buf = [0u8; 16];
        for (i, b) in inner.bytes().enumerate() {
            buf[i] = b.to_ascii_uppercase();
        }
        let up = std::str::from_utf8(&buf[..inner.len()]).unwrap_or("");
        return classify_upper_symbol(up);
    }
    let up = inner.to_ascii_uppercase();
    classify_upper_symbol(&up)
}

fn classify_upper_symbol<'a>(up: &str) -> TokenKind<'a> {
    match up {
        "T" => return TokenKind::Tap,
        "Q" => return TokenKind::Untap,
        "E" => return TokenKind::Energy,
        "P" => return TokenKind::Pawprint,
        "CHAOS" => return TokenKind::Chaos,
        "S" => return TokenKind::ManaSymbol(ManaSymbolKind::Snow),
        "X" => return TokenKind::ManaSymbol(ManaSymbolKind::Variable('X')),
        "Y" => return TokenKind::ManaSymbol(ManaSymbolKind::Variable('Y')),
        "Z" => return TokenKind::ManaSymbol(ManaSymbolKind::Variable('Z')),
        _ => {}
    }
    if !up.is_empty()
        && up.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = up.parse::<u32>()
    {
        return TokenKind::ManaSymbol(ManaSymbolKind::Generic(n));
    }
    if let Some(c) = parse_mana_color(up) {
        return TokenKind::ManaSymbol(ManaSymbolKind::Colored(c));
    }
    let mut parts = up.split('/');
    if let (Some(a), Some(b), third, None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    {
        match (a, b, third) {
            ("2", c, None) => {
                if let Some(mc) = parse_mana_color(c) {
                    return TokenKind::ManaSymbol(ManaSymbolKind::TwoBrid(mc));
                }
            }
            (c, "P", None) => {
                if let Some(mc) = parse_mana_color(c) {
                    return TokenKind::ManaSymbol(ManaSymbolKind::Phyrexian(mc));
                }
            }
            ("C", c, None) => {
                if let Some(mc) = parse_mana_color(c) {
                    return TokenKind::ManaSymbol(ManaSymbolKind::ColorlessHybrid(mc));
                }
            }
            (ca, cb, None) => {
                if let (Some(ma), Some(mb)) = (parse_mana_color(ca), parse_mana_color(cb)) {
                    return TokenKind::ManaSymbol(ManaSymbolKind::Hybrid(ma, mb));
                }
            }
            (ca, cb, Some("P")) => {
                if let (Some(ma), Some(mb)) = (parse_mana_color(ca), parse_mana_color(cb)) {
                    return TokenKind::ManaSymbol(ManaSymbolKind::HybridPhyrexian(ma, mb));
                }
            }
            _ => {}
        }
    }
    TokenKind::ManaSymbol(ManaSymbolKind::Unknown)
}

fn parse_number_word(w: &str) -> Option<u32> {
    if w.eq_ignore_ascii_case("zero") {
        Some(0)
    } else if w.eq_ignore_ascii_case("one") {
        Some(1)
    } else if w.eq_ignore_ascii_case("two") {
        Some(2)
    } else if w.eq_ignore_ascii_case("three") {
        Some(3)
    } else if w.eq_ignore_ascii_case("four") {
        Some(4)
    } else if w.eq_ignore_ascii_case("five") {
        Some(5)
    } else if w.eq_ignore_ascii_case("six") {
        Some(6)
    } else if w.eq_ignore_ascii_case("seven") {
        Some(7)
    } else if w.eq_ignore_ascii_case("eight") {
        Some(8)
    } else if w.eq_ignore_ascii_case("nine") {
        Some(9)
    } else if w.eq_ignore_ascii_case("ten") {
        Some(10)
    } else if w.eq_ignore_ascii_case("eleven") {
        Some(11)
    } else if w.eq_ignore_ascii_case("twelve") {
        Some(12)
    } else if w.eq_ignore_ascii_case("thirteen") {
        Some(13)
    } else if w.eq_ignore_ascii_case("fourteen") {
        Some(14)
    } else if w.eq_ignore_ascii_case("fifteen") {
        Some(15)
    } else if w.eq_ignore_ascii_case("sixteen") {
        Some(16)
    } else if w.eq_ignore_ascii_case("seventeen") {
        Some(17)
    } else if w.eq_ignore_ascii_case("eighteen") {
        Some(18)
    } else if w.eq_ignore_ascii_case("nineteen") {
        Some(19)
    } else if w.eq_ignore_ascii_case("twenty") {
        Some(20)
    } else {
        None
    }
}

#[inline]
fn is_word_boundary_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn parse_signed_part(s: &str) -> Option<(SignedPart, bool, usize)> {
    let mut pos = 0usize;
    let mut sign: Option<i32> = None;
    if let Some(rest) = s.strip_prefix('+') {
        sign = Some(1);
        pos += 1;
        let _ = rest;
    } else if let Some(rest) = s.strip_prefix('-') {
        sign = Some(-1);
        pos += 1;
        let _ = rest;
    } else if let Some(rest) = s.strip_prefix('−') {
        sign = Some(-1);
        pos += '−'.len_utf8();
        let _ = rest;
    }

    let tail = &s[pos..];
    if tail.starts_with('X') || tail.starts_with('x') {
        let part = match sign {
            Some(-1) => SignedPart::MinusX,
            _ => SignedPart::PlusX,
        };
        return Some((part, sign.is_some(), pos + 1));
    }

    let digit_len = tail.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let val: i32 = tail[..digit_len].parse().ok()?;
    let signed_val = match sign {
        Some(-1) => val.checked_neg()?,
        _ => val,
    };
    Some((SignedPart::Int(signed_val), sign.is_some(), pos + digit_len))
}

fn try_parse_signed_pt(s: &str) -> Option<(SignedPart, SignedPart, usize)> {
    let (power, left_signed, left_len) = parse_signed_part(s)?;
    let after_left = &s[left_len..];
    let after_slash = after_left.strip_prefix('/')?;
    let (toughness, right_signed, right_len) = parse_signed_part(after_slash)?;
    if !left_signed && !right_signed {
        return None;
    }
    let total_len = left_len + 1 + right_len;
    if s[total_len..]
        .chars()
        .next()
        .is_some_and(is_word_boundary_char)
    {
        return None;
    }
    Some((power, toughness, total_len))
}

fn try_parse_signed_number(s: &str) -> Option<(i32, usize)> {
    let (sign, sign_len) = if s.starts_with('+') {
        (1i32, 1usize)
    } else if s.starts_with('-') {
        (-1i32, 1usize)
    } else if s.starts_with('−') {
        (-1i32, '−'.len_utf8())
    } else {
        return None;
    };
    let tail = &s[sign_len..];
    let digit_len = tail.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let total_len = sign_len + digit_len;
    if s[total_len..]
        .chars()
        .next()
        .is_some_and(is_word_boundary_char)
    {
        return None;
    }
    let val: i32 = tail[..digit_len].parse().ok()?;
    Some((val.checked_mul(sign)?, total_len))
}

/// Tokenizes normalized Oracle text into spanned tokens without regexes.
pub fn tokenize<'a>(input: &'a str, base_offset: u32) -> Vec<Token<'a>> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let ch = input[i..].chars().next().unwrap();
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }

        // 1. Braced symbols {...}
        if ch == '{' {
            if let Some(rel_end) = input[i + 1..].find('}') {
                let inner = &input[i + 1..i + 1 + rel_end];
                let end = i + 2 + rel_end;
                let kind = classify_braced_symbol(inner);
                tokens.push(Token {
                    kind,
                    raw: &input[i..end],
                    span: Span {
                        start: base_offset + i as u32,
                        end: base_offset + end as u32,
                    },
                });
                i = end;
                continue;
            }
            tokens.push(Token {
                kind: TokenKind::Word(&input[i..i + 1]),
                raw: &input[i..i + 1],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + (i + 1) as u32,
                },
            });
            i += 1;
            continue;
        }

        // 2. Quoted spans "..." or “...”
        if ch == '"' || ch == '“' {
            let close_ch = if ch == '"' { '"' } else { '”' };
            let open_len = ch.len_utf8();
            let after_open = &input[i + open_len..];
            let (inner_end_rel, total_end) = if let Some(rel_close) = after_open.find(close_ch) {
                (rel_close, i + open_len + rel_close + close_ch.len_utf8())
            } else if let Some(rel_nl) = after_open.find('\n') {
                (rel_nl, i + open_len + rel_nl)
            } else {
                (after_open.len(), input.len())
            };
            let inner = &after_open[..inner_end_rel];
            tokens.push(Token {
                kind: TokenKind::QuotedSpan(inner),
                raw: &input[i..total_end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + total_end as u32,
                },
            });
            i = total_end;
            continue;
        }

        // 3. SelfRef ~ and Infinity ∞
        if ch == '~' {
            let end = i + 1;
            tokens.push(Token {
                kind: TokenKind::SelfRef,
                raw: &input[i..end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }
        if ch == '∞' {
            let end = i + ch.len_utf8();
            tokens.push(Token {
                kind: TokenKind::Infinity,
                raw: &input[i..end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        // 4. SignedPt and SignedNumber (only if not preceded by word char)
        let prev_is_word = input[..i]
            .chars()
            .next_back()
            .is_some_and(is_word_boundary_char);

        if !prev_is_word
            && (matches!(ch, '+' | '-' | '−' | 'X' | 'x') || ch.is_ascii_digit())
            && let Some((power, toughness, len)) = try_parse_signed_pt(&input[i..])
        {
            let end = i + len;
            tokens.push(Token {
                kind: TokenKind::SignedPt { power, toughness },
                raw: &input[i..end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        if !prev_is_word
            && matches!(ch, '+' | '-' | '−')
            && let Some((val, len)) = try_parse_signed_number(&input[i..])
        {
            let end = i + len;
            tokens.push(Token {
                kind: TokenKind::SignedNumber(val),
                raw: &input[i..end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        // 5. Unsigned Number
        if ch.is_ascii_digit() {
            let digit_len = input[i..]
                .bytes()
                .take_while(|b| b.is_ascii_digit())
                .count();
            let end = i + digit_len;
            let raw = &input[i..end];
            let kind = match raw.parse::<u32>() {
                Ok(n) => TokenKind::Number(n),
                Err(_) => TokenKind::Word(raw),
            };
            tokens.push(Token {
                kind,
                raw,
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        // 6. Punctuation
        if let Some(p) = match ch {
            ',' => Some(Punct::Comma),
            '.' => Some(Punct::Period),
            ':' => Some(Punct::Colon),
            ';' => Some(Punct::Semicolon),
            '—' => Some(Punct::EmDash),
            '–' => Some(Punct::EnDash),
            '-' | '−' => Some(Punct::Hyphen),
            '/' => Some(Punct::Slash),
            '(' => Some(Punct::LParen),
            ')' => Some(Punct::RParen),
            '[' => Some(Punct::LBracket),
            ']' => Some(Punct::RBracket),
            '•' => Some(Punct::Bullet),
            '+' => Some(Punct::Plus),
            '!' => Some(Punct::Bang),
            '?' => Some(Punct::Question),
            _ => None,
        } {
            let end = i + ch.len_utf8();
            tokens.push(Token {
                kind: TokenKind::Punctuation(p),
                raw: &input[i..end],
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        // 7. Possessive `'s` / `’s` after `~`, `)`, etc.
        if (ch == '\'' || ch == '’') && input[i + ch.len_utf8()..].starts_with(['s', 'S']) {
            let after_s_idx = i + ch.len_utf8() + 1;
            let next_is_word = input[after_s_idx..]
                .chars()
                .next()
                .is_some_and(is_word_boundary_char);
            if !next_is_word {
                let raw = &input[i..after_s_idx];
                tokens.push(Token {
                    kind: TokenKind::Word(raw),
                    raw,
                    span: Span {
                        start: base_offset + i as u32,
                        end: base_offset + after_s_idx as u32,
                    },
                });
                i = after_s_idx;
                continue;
            }
        }

        // 8. Words (alphabetic/underscore + internal apostrophes followed by alpha)
        if ch.is_alphabetic() || ch == '_' {
            let mut end = i + ch.len_utf8();
            while end < bytes.len() {
                let next = input[end..].chars().next().unwrap();
                if (next.is_alphanumeric() || next == '_')
                    || ((next == '\'' || next == '’')
                        && input[end + next.len_utf8()..]
                            .chars()
                            .next()
                            .is_some_and(char::is_alphabetic))
                {
                    end += next.len_utf8();
                } else {
                    break;
                }
            }
            let raw = &input[i..end];
            let kind = if raw == "X" || raw == "x" {
                TokenKind::X
            } else if let Some(n) = parse_number_word(raw) {
                TokenKind::NumberWord(n)
            } else {
                TokenKind::Word(raw)
            };
            tokens.push(Token {
                kind,
                raw,
                span: Span {
                    start: base_offset + i as u32,
                    end: base_offset + end as u32,
                },
            });
            i = end;
            continue;
        }

        // 9. Any other symbol (single char fallback so nothing is silently dropped)
        let end = i + ch.len_utf8();
        let raw = &input[i..end];
        tokens.push(Token {
            kind: TokenKind::Word(raw),
            raw,
            span: Span {
                start: base_offset + i as u32,
                end: base_offset + end as u32,
            },
        });
        i = end;
    }

    tokens
}
