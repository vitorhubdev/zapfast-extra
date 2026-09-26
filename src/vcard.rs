//! Shared contact cards (vCards), read when a contact message is drawn.
//!
//! WhatsApp carries a shared contact as the vCard text the phone wrote; a
//! contacts array arrives as several cards joined together. The archive
//! keeps that text as it came, so old messages parse exactly like new ones.
//! Parsing is pure and cheap: no lookups, no network.

/// Fewest digits in a complete international number (country code included).
const MIN_DIGITS: usize = 7;
/// Most digits an international number may have (E.164).
const MAX_DIGITS: usize = 15;

/// One person on a shared contact card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Card {
    /// The formatted name (`FN`), when the card has one.
    pub name: Option<String>,
    /// Every phone number on the card, in card order, without repeats.
    pub numbers: Vec<Number>,
}

impl Card {
    /// The card's name, or "Contact" when it has none.
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("Contact")
    }
}

/// A phone number from a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Number {
    /// The number as the card wrote it.
    pub text: String,
    /// International digits a chat can open with, or why there are none.
    pub dial: Result<String, Unusable>,
}

impl Number {
    /// The chat id for this number, when it is usable.
    pub fn chat_id(&self) -> Option<String> {
        self.dial
            .as_ref()
            .ok()
            .map(|digits| format!("{digits}@s.whatsapp.net"))
    }
}

/// Why a number on a card cannot open a chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// Written in local form. Guessing a country code could reach a stranger.
    NoCountryCode,
    /// Too short or too long to be a complete international number.
    Incomplete,
}

impl Unusable {
    /// A short explanation for the card.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoCountryCode => "no country code",
            Self::Incomplete => "incomplete number",
        }
    }
}

/// Reads every card in `text`, in order.
///
/// Text without a `BEGIN:VCARD` line is read as a single card.
pub fn parse(text: &str) -> Vec<Card> {
    let lines = unfold(text);
    let framed = lines.iter().any(|line| is_marker(line, "BEGIN"));
    let mut cards = Vec::new();
    let mut current = (!framed).then(Card::default);
    for line in &lines {
        if is_marker(line, "BEGIN") {
            cards.extend(current.replace(Card::default()));
        } else if is_marker(line, "END") {
            cards.extend(current.take());
        } else if let Some(card) = current.as_mut() {
            read_property(card, line);
        }
    }
    cards.extend(current);
    cards.retain(|card| card.name.is_some() || !card.numbers.is_empty());
    // Cards arrive over the network, so dedup in linear time: a hostile
    // card with thousands of numbers must not cost a quadratic scan on
    // every frame the bubble is visible.
    for card in &mut cards {
        let mut seen = std::collections::HashSet::new();
        card.numbers
            .retain(|number| seen.insert(number_key(number)));
    }
    cards
}

/// Splits lines on LF or CRLF and joins folded continuations.
///
/// A line starting with a space or tab continues the one before it. A
/// quoted-printable value (vCard 2.1) ending in `=` also continues.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut soft_break = false;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let continued = match raw.strip_prefix([' ', '\t']) {
            Some(rest) => Some(rest),
            None if soft_break => Some(raw),
            None => None,
        };
        match (continued, lines.last_mut()) {
            (Some(rest), Some(last)) => {
                if soft_break {
                    last.pop();
                }
                last.push_str(rest);
            }
            _ => lines.push(raw.to_owned()),
        }
        soft_break = lines.last().is_some_and(|last| {
            last.ends_with('=') && last.to_ascii_uppercase().contains("QUOTED-PRINTABLE")
        });
    }
    lines
}

/// Whether `line` is `BEGIN:VCARD` or `END:VCARD` (for `which`).
fn is_marker(line: &str, which: &str) -> bool {
    line.trim().split_once(':').is_some_and(|(name, value)| {
        name.trim().eq_ignore_ascii_case(which) && value.trim().eq_ignore_ascii_case("VCARD")
    })
}

/// Files one property line into `card`.
fn read_property(card: &mut Card, line: &str) {
    let Some((head, value)) = split_value(line) else {
        return;
    };
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or_default().trim();
    // `item1.TEL` groups a property with its label; the group is irrelevant.
    let name = name.rsplit('.').next().unwrap_or(name);
    let params: Vec<&str> = parts.collect();
    if name.eq_ignore_ascii_case("FN") {
        if card.name.is_none() {
            let decoded = if quoted_printable(&params) {
                decode_quoted_printable(value)
            } else {
                value.to_owned()
            };
            let name = unescape(&decoded);
            let name = name.trim();
            if !name.is_empty() {
                card.name = Some(name.to_owned());
            }
        }
    } else if name.eq_ignore_ascii_case("TEL") {
        // A vCard 2.1 writer may encode the number too; decode it before
        // reading tel:, digits, waid fallback, or extensions.
        let value = if quoted_printable(&params) {
            decode_quoted_printable(value)
        } else {
            value.to_owned()
        };
        // Duplicates leave with the linear pass at the end of `parse`.
        if let Some(number) = read_number(&value, &params) {
            card.numbers.push(number);
        }
    }
}

/// Splits `NAME;PARAMS:VALUE` at the first colon outside double quotes.
fn split_value(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some((&line[..index], &line[index + 1..])),
            _ => {}
        }
    }
    None
}

/// Reads a `TEL` value and its parameters.
///
/// A `waid=` parameter is WhatsApp's own record of the account behind the
/// number, so it wins over the written number when it is well formed.
fn read_number(value: &str, params: &[&str]) -> Option<Number> {
    let value = value.trim();
    // A `tel:` URI may carry its own parameters (`;ext=12`) after the number.
    let value = match value.get(..4) {
        Some(scheme) if scheme.eq_ignore_ascii_case("tel:") => &value[4..],
        _ => value,
    };
    let text = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let waid = params.iter().find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim().eq_ignore_ascii_case("waid").then(|| {
            value
                .trim()
                .trim_matches('"')
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
    });
    let waid = waid
        .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .and_then(|digits| international(&digits).ok());
    if text.is_empty() && waid.is_none() {
        return None;
    }
    let dial = match waid {
        Some(digits) => Ok(digits),
        None => dial(&text),
    };
    let text = if text.is_empty() {
        dial.as_ref()
            .map(|digits| format!("+{digits}"))
            .unwrap_or_default()
    } else {
        text
    };
    Some(Number { text, dial })
}

/// International digits for a written number.
///
/// Only a leading `+` counts as evidence of a country code; a number in
/// local form stays unusable rather than gaining a guessed one.
fn dial(text: &str) -> Result<String, Unusable> {
    // An extension or dialling pause is not part of the number.
    let number = text
        .split(|c: char| c.is_ascii_alphabetic() || c == ',')
        .next()
        .unwrap_or_default()
        // "+44 (0) 20 ..." marks the trunk zero dialled only at home.
        .replace("(0)", "");
    let number = number.trim_start();
    match number.strip_prefix('+') {
        Some(rest) => international(&digits(rest)),
        None if digits(number).len() < MIN_DIGITS => Err(Unusable::Incomplete),
        None => Err(Unusable::NoCountryCode),
    }
}

/// Checks that `digits` can be a whole international number.
fn international(digits: &str) -> Result<String, Unusable> {
    // Country codes never start with zero.
    if digits.starts_with('0') || !(MIN_DIGITS..=MAX_DIGITS).contains(&digits.len()) {
        return Err(Unusable::Incomplete);
    }
    Ok(digits.to_owned())
}

fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

/// What makes two numbers on one card the same number.
fn number_key(number: &Number) -> String {
    match &number.dial {
        Ok(digits) => format!("+{digits}"),
        Err(_) => digits(&number.text),
    }
}

fn quoted_printable(params: &[&str]) -> bool {
    params.iter().any(|param| {
        let value = param.split_once('=').map_or(*param, |(_, value)| value);
        value.trim().eq_ignore_ascii_case("QUOTED-PRINTABLE")
    })
}

/// Decodes `=XX` escapes (vCard 2.1), reading the bytes as UTF-8.
fn decode_quoted_printable(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'='
            && let Some(byte) = value
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Undoes vCard text escapes (`\,`, `\;`, `\\`, `\n`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => out.push(' '),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usable(card: &Card) -> Vec<&str> {
        card.numbers
            .iter()
            .filter_map(|number| number.dial.as_deref().ok())
            .collect()
    }

    #[test]
    fn a_simple_card_has_its_name_and_number() {
        let cards = parse(
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada Lovelace\nTEL;TYPE=CELL:+1 555 010 0199\nEND:VCARD",
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name(), "Ada Lovelace");
        assert_eq!(usable(&cards[0]), ["15550100199"]);
        assert_eq!(cards[0].numbers[0].text, "+1 555 010 0199");
        assert_eq!(
            cards[0].numbers[0].chat_id().as_deref(),
            Some("15550100199@s.whatsapp.net")
        );
    }

    #[test]
    fn grouped_numbers_are_read_and_every_number_is_kept() {
        let cards = parse(
            "BEGIN:VCARD\nVERSION:3.0\nFN:Grace Hopper\n\
             item1.TEL:+1 (555) 010-0142\nitem1.X-ABLabel:Work\n\
             ITEM2.tel;type=HOME:+1 555 010 0143\nEND:VCARD",
        );
        assert_eq!(usable(&cards[0]), ["15550100142", "15550100143"]);
    }

    #[test]
    fn the_waid_parameter_is_authoritative() {
        // WhatsApp writes the account's digits in `waid`, which may differ
        // from the number as displayed (an older account without the 9).
        let cards = parse(
            "BEGIN:VCARD\nVERSION:3.0\nN:;Lin;;;\nFN:Lin\n\
             item1.TEL;waid=15550100177:+1 555-0100-9177\nEND:VCARD",
        );
        assert_eq!(usable(&cards[0]), ["15550100177"]);
        assert_eq!(cards[0].numbers[0].text, "+1 555-0100-9177");
        // A waid rescues a number written in local form.
        let local =
            parse("BEGIN:VCARD\nFN:Lin\nTEL;type=CELL;waid=15550100177:555 0177\nEND:VCARD");
        assert_eq!(usable(&local[0]), ["15550100177"]);
        // A malformed waid falls back to the written number.
        let broken = parse("BEGIN:VCARD\nFN:Lin\nTEL;waid=12x:+1 555 010 0188\nEND:VCARD");
        assert_eq!(usable(&broken[0]), ["15550100188"]);
    }

    #[test]
    fn existing_contact_digits_pass_through_untouched() {
        // Digits the phone already resolved come back exactly, with no
        // reformatting, trunk rewriting, or added or dropped digits.
        for digits in ["15550100123", "155501001234", "1555010012345"] {
            let card = format!("BEGIN:VCARD\nFN:Known\nTEL;waid={digits}:+{digits}\nEND:VCARD");
            assert_eq!(usable(&parse(&card)[0]), [digits]);
            let plain = format!("BEGIN:VCARD\nFN:Known\nTEL:+{digits}\nEND:VCARD");
            assert_eq!(usable(&parse(&plain)[0]), [digits]);
        }
    }

    #[test]
    fn several_cards_keep_their_own_numbers() {
        // The worker joins a contacts array with newlines.
        let joined = [
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada\nTEL:+1 555 010 0101\nEND:VCARD",
            "BEGIN:VCARD\nVERSION:3.0\nFN:Bea\nTEL:+1 555 010 0102\nTEL:+1 555 010 0103\nEND:VCARD",
            "BEGIN:VCARD\nVERSION:3.0\nFN:Cy\nEND:VCARD",
        ]
        .join("\n");
        let cards = parse(&joined);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].name(), "Ada");
        assert_eq!(usable(&cards[0]), ["15550100101"]);
        assert_eq!(cards[1].name(), "Bea");
        assert_eq!(usable(&cards[1]), ["15550100102", "15550100103"]);
        assert_eq!(cards[2].name(), "Cy");
        assert!(cards[2].numbers.is_empty());
    }

    #[test]
    fn crlf_and_folded_lines_are_joined() {
        let cards = parse(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Katherine\r\n  Johnson\r\n\
             TEL;TYPE=CELL;\r\n\twaid=15550100155:+1 555 \r\n 010 0155\r\nEND:VCARD\r\n",
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name(), "Katherine Johnson");
        assert_eq!(usable(&cards[0]), ["15550100155"]);
        assert_eq!(cards[0].numbers[0].text, "+1 555 010 0155");
    }

    #[test]
    fn short_and_local_numbers_are_rejected_without_guessing_a_country() {
        let cards = parse(
            "BEGIN:VCARD\nFN:Local\nTEL:(555) 010-0166\nTEL:0555 010 0167\n\
             TEL:190\nTEL:+1 555\nTEL:+0 555 010 0168\nEND:VCARD",
        );
        let reasons: Vec<_> = cards[0]
            .numbers
            .iter()
            .map(|number| number.dial.clone())
            .collect();
        assert_eq!(
            reasons,
            [
                Err(Unusable::NoCountryCode),
                Err(Unusable::NoCountryCode),
                Err(Unusable::Incomplete),
                Err(Unusable::Incomplete),
                Err(Unusable::Incomplete),
            ]
        );
        assert!(cards[0].numbers.iter().all(|n| n.chat_id().is_none()));
        // The written form is kept for display.
        assert_eq!(cards[0].numbers[0].text, "(555) 010-0166");
    }

    #[test]
    fn tel_uris_extensions_and_trunk_zeroes_are_handled() {
        let cards = parse(
            "BEGIN:VCARD\nVERSION:4.0\nFN:Uri\n\
             TEL;VALUE=uri;TYPE=\"voice,cell\":tel:+1-555-010-0170;ext=12\n\
             TEL:+44 (0) 20 7946 0171\nTEL:+1 555 010 0172 ext. 4\nEND:VCARD",
        );
        assert_eq!(
            usable(&cards[0]),
            ["15550100170", "442079460171", "15550100172"]
        );
    }

    #[test]
    fn repeated_numbers_appear_once() {
        let cards = parse(
            "BEGIN:VCARD\nFN:Twice\nTEL;waid=15550100180:+1 555 010 0180\n\
             item1.TEL:+1 (555) 010-0180\nEND:VCARD",
        );
        assert_eq!(cards[0].numbers.len(), 1);
    }

    #[test]
    fn names_are_unescaped_and_decoded() {
        let cards = parse(
            "BEGIN:VCARD\nVERSION:2.1\n\
             FN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Jo=C3=A3o =\nda Silva\n\
             TEL;CELL:+1 555 010 0190\nEND:VCARD\n\
             BEGIN:VCARD\nFN:Smith\\, Jane\nEND:VCARD",
        );
        assert_eq!(cards[0].name(), "Jo\u{e3}o da Silva");
        assert_eq!(usable(&cards[0]), ["15550100190"]);
        assert_eq!(cards[1].name(), "Smith, Jane");
    }

    #[test]
    fn a_card_without_a_name_falls_back() {
        let cards = parse("BEGIN:VCARD\nTEL:+1 555 010 0195\nEND:VCARD");
        assert_eq!(cards[0].name, None);
        assert_eq!(cards[0].name(), "Contact");
    }

    #[test]
    fn empty_and_unframed_text() {
        assert!(parse("").is_empty());
        assert!(parse("BEGIN:VCARD\nVERSION:3.0\nEND:VCARD").is_empty());
        let unframed = parse("FN:Loose\nTEL:+1 555 010 0199");
        assert_eq!(unframed.len(), 1);
        assert_eq!(usable(&unframed[0]), ["15550100199"]);
        // A truncated card still counts.
        let truncated = parse("BEGIN:VCARD\nFN:Cut\nTEL:+1 555 010 0198");
        assert_eq!(usable(&truncated[0]), ["15550100198"]);
    }

    #[test]
    fn a_quoted_printable_number_reads_like_the_plain_one() {
        let plain = parse("BEGIN:VCARD\nFN:QP\nTEL:+15550100199\nEND:VCARD");
        let encoded =
            parse("BEGIN:VCARD\nFN:QP\nTEL;ENCODING=QUOTED-PRINTABLE:=2B15550100199\nEND:VCARD");
        assert_eq!(usable(&encoded[0]), ["15550100199"]);
        assert_eq!(
            encoded[0].numbers[0].chat_id(),
            plain[0].numbers[0].chat_id()
        );
        // A valid waid still wins over the encoded value.
        let waid = parse(
            "BEGIN:VCARD\nFN:QP\nTEL;ENCODING=QUOTED-PRINTABLE;waid=15550100142:=2B15550100199\nEND:VCARD",
        );
        assert_eq!(usable(&waid[0]), ["15550100142"]);
    }
}
