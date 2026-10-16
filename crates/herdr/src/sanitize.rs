//! Text that is about to be typed into a live terminal must not be able to
//! drive it. Shared by every path that hands text to herdr `agent.send`.

/// Makes text safe to type into a live terminal: newlines become LF, and
/// everything that could drive the terminal rather than be read is dropped:
/// ESC with its whole CSI/OSC/DCS/SOS/PM/APC sequence, every other C0
/// control except `\n` and `\t`, DEL, the C1 controls, the Unicode line and
/// paragraph separators, and the zero-width and bidi format characters that
/// could hide or reorder what the agent sees.
pub fn sanitize_for_terminal(text: &str) -> String {
    let text = normalize_newlines(text);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1B}' => skip_escape_sequence(&mut chars),
            '\n' | '\t' => out.push(character),
            '\u{00}'..='\u{1F}' | '\u{7F}' => {}
            '\u{80}'..='\u{9F}' | '\u{2028}' | '\u{2029}' => {}
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => {}
            '\u{FEFF}' => {}
            _ => out.push(character),
        }
    }
    out
}

/// Consumes the rest of an escape sequence whose ESC was just read.
fn skip_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(kind) = chars.next() else {
        return;
    };
    match kind {
        // CSI: parameter and intermediate bytes, then one final byte 0x40..=0x7E.
        '[' => {
            for character in chars.by_ref() {
                if ('\u{40}'..='\u{7E}').contains(&character) {
                    break;
                }
            }
        }
        // OSC ends with BEL or ST (ESC \); DCS, SOS, PM and APC end with ST.
        ']' | 'P' | 'X' | '^' | '_' => {
            let mut previous_was_escape = false;
            for character in chars.by_ref() {
                if character == '\u{07}' && kind == ']' {
                    break;
                }
                if previous_was_escape && character == '\\' {
                    break;
                }
                previous_was_escape = character == '\u{1B}';
            }
        }
        // Two-character escapes (ESC c, ESC 7, ESC ( B, ...): `kind` was the
        // second byte; a following intermediate byte 0x20..=0x2F takes one more.
        _ => {
            if ('\u{20}'..='\u{2F}').contains(&kind) {
                chars.next();
            }
        }
    }
}

/// CRLF and lone CR become LF so a terminal never sees a carriage return
/// that could overwrite or split what we hand to an agent.
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
