//! The multilingual Whisper vocabulary: token ids to text, and where the special tokens sit.
//!
//! Every multilingual checkpoint shares the same 50257 byte-pair tokens (packed into
//! `assets/multilingual.vocab` by `scripts/whisper-export/vocab.py`) and appends its specials
//! after them in a fixed order, so the special ids follow from the vocabulary size alone:
//! `<|endoftext|>`, `<|startoftranscript|>`, one token per language, six control tokens, then
//! 1501 timestamps `<|0.00|>` .. `<|30.00|>`. large-v3 and turbo carry 100 languages (Cantonese
//! was added), everything before them 99 — which is why their control tokens sit one id higher.

use crate::error::{Error, Result};

const PACKED: &[u8] = include_bytes!("../assets/multilingual.vocab");
const N_TEXT: u32 = 50257;
const N_TIMESTAMPS: u32 = 1501;
/// Seconds one timestamp token step is worth.
pub const TIMESTAMP_SECONDS: f64 = 0.02;

/// Language codes in token order. The 100th, `yue`, exists only in the large-v3 family.
pub const LANGUAGES: [(&str, &str); 100] = [
    ("en", "English"),
    ("zh", "Chinese"),
    ("de", "German"),
    ("es", "Spanish"),
    ("ru", "Russian"),
    ("ko", "Korean"),
    ("fr", "French"),
    ("ja", "Japanese"),
    ("pt", "Portuguese"),
    ("tr", "Turkish"),
    ("pl", "Polish"),
    ("ca", "Catalan"),
    ("nl", "Dutch"),
    ("ar", "Arabic"),
    ("sv", "Swedish"),
    ("it", "Italian"),
    ("id", "Indonesian"),
    ("hi", "Hindi"),
    ("fi", "Finnish"),
    ("vi", "Vietnamese"),
    ("he", "Hebrew"),
    ("uk", "Ukrainian"),
    ("el", "Greek"),
    ("ms", "Malay"),
    ("cs", "Czech"),
    ("ro", "Romanian"),
    ("da", "Danish"),
    ("hu", "Hungarian"),
    ("ta", "Tamil"),
    ("no", "Norwegian"),
    ("th", "Thai"),
    ("ur", "Urdu"),
    ("hr", "Croatian"),
    ("bg", "Bulgarian"),
    ("lt", "Lithuanian"),
    ("la", "Latin"),
    ("mi", "Maori"),
    ("ml", "Malayalam"),
    ("cy", "Welsh"),
    ("sk", "Slovak"),
    ("te", "Telugu"),
    ("fa", "Persian"),
    ("lv", "Latvian"),
    ("bn", "Bengali"),
    ("sr", "Serbian"),
    ("az", "Azerbaijani"),
    ("sl", "Slovenian"),
    ("kn", "Kannada"),
    ("et", "Estonian"),
    ("mk", "Macedonian"),
    ("br", "Breton"),
    ("eu", "Basque"),
    ("is", "Icelandic"),
    ("hy", "Armenian"),
    ("ne", "Nepali"),
    ("mn", "Mongolian"),
    ("bs", "Bosnian"),
    ("kk", "Kazakh"),
    ("sq", "Albanian"),
    ("sw", "Swahili"),
    ("gl", "Galician"),
    ("mr", "Marathi"),
    ("pa", "Punjabi"),
    ("si", "Sinhala"),
    ("km", "Khmer"),
    ("sn", "Shona"),
    ("yo", "Yoruba"),
    ("so", "Somali"),
    ("af", "Afrikaans"),
    ("oc", "Occitan"),
    ("ka", "Georgian"),
    ("be", "Belarusian"),
    ("tg", "Tajik"),
    ("sd", "Sindhi"),
    ("gu", "Gujarati"),
    ("am", "Amharic"),
    ("yi", "Yiddish"),
    ("lo", "Lao"),
    ("uz", "Uzbek"),
    ("fo", "Faroese"),
    ("ht", "Haitian Creole"),
    ("ps", "Pashto"),
    ("tk", "Turkmen"),
    ("nn", "Nynorsk"),
    ("mt", "Maltese"),
    ("sa", "Sanskrit"),
    ("lb", "Luxembourgish"),
    ("my", "Myanmar"),
    ("bo", "Tibetan"),
    ("tl", "Tagalog"),
    ("mg", "Malagasy"),
    ("as", "Assamese"),
    ("tt", "Tatar"),
    ("haw", "Hawaiian"),
    ("ln", "Lingala"),
    ("ha", "Hausa"),
    ("ba", "Bashkir"),
    ("jw", "Javanese"),
    ("su", "Sundanese"),
    ("yue", "Cantonese"),
];

/// Byte-pair tokens OpenAI's decoder never samples: brackets, musical notes and other symbols
/// that mark non-speech, which Whisper otherwise emits as hallucinated annotations. Identical
/// across multilingual checkpoints (`suppress_tokens` in their generation configs, minus the
/// special tokens, which [`Vocab::suppressed`] adds for the model at hand).
const NON_SPEECH: [u32; 82] = [
    1, 2, 7, 8, 9, 10, 14, 25, 26, 27, 28, 29, 31, 58, 59, 60, 61, 62, 63, 90, 91, 92, 93, 359, 503, 522, 542, 873, 893, 902, 918,
    922, 931, 1350, 1853, 1982, 2460, 2627, 3246, 3253, 3268, 3536, 3846, 3961, 4183, 4667, 6585, 6647, 7273, 9061, 9383, 10428,
    10929, 11938, 12033, 12331, 12562, 13793, 14157, 14635, 15265, 15618, 16553, 16604, 18362, 18956, 20075, 21675, 22520, 26130,
    26161, 26435, 28279, 29464, 31650, 32302, 32470, 36865, 42863, 47425, 49870, 50254,
];

/// The space token; sampling it first would open a transcript with a blank.
const BLANK: u32 = 220;

/// Token table plus the special ids of one model size.
pub struct Vocab {
    /// Byte offsets into [`PACKED`]: token `i` is `PACKED[starts[i]..starts[i + 1] - 1]`
    /// once the next token's length byte is discounted.
    starts: Vec<u32>,
    n_vocab: u32,
    n_langs: u32,
    pub eot: u32,
    pub sot: u32,
    pub translate: u32,
    pub transcribe: u32,
    pub sot_lm: u32,
    pub sot_prev: u32,
    pub no_speech: u32,
    pub no_timestamps: u32,
    pub timestamp_begin: u32,
}

impl Vocab {
    /// The vocabulary of a multilingual model whose logits are `n_vocab` wide.
    pub fn multilingual(n_vocab: usize) -> Result<Self> {
        let fixed = N_TEXT + 2 + 6 + N_TIMESTAMPS;
        let n_langs = (n_vocab as u32).checked_sub(fixed).filter(|n| (99..=LANGUAGES.len() as u32).contains(n)).ok_or(Error::UnknownVocab(n_vocab))?;
        let mut starts = Vec::with_capacity(N_TEXT as usize + 1);
        let mut at = 0usize;
        while at < PACKED.len() {
            starts.push(at as u32 + 1);
            at += 1 + PACKED[at] as usize;
        }
        // One past the last token's bytes, plus the length byte the lookup subtracts.
        starts.push(at as u32 + 1);
        debug_assert_eq!(starts.len(), N_TEXT as usize + 1);
        let control = N_TEXT + 2 + n_langs;
        Ok(Self {
            starts,
            n_vocab: n_vocab as u32,
            n_langs,
            eot: N_TEXT,
            sot: N_TEXT + 1,
            translate: control,
            transcribe: control + 1,
            sot_lm: control + 2,
            sot_prev: control + 3,
            no_speech: control + 4,
            no_timestamps: control + 5,
            timestamp_begin: control + 6,
        })
    }

    pub fn n_vocab(&self) -> usize {
        self.n_vocab as usize
    }

    /// Ids of the language tokens, in [`LANGUAGES`] order.
    pub fn language_tokens(&self) -> std::ops::Range<u32> {
        self.sot + 1..self.sot + 1 + self.n_langs
    }

    pub fn language_token(&self, code: &str) -> Result<u32> {
        let index = LANGUAGES[..self.n_langs as usize].iter().position(|(c, _)| *c == code).ok_or_else(|| Error::UnknownLanguage(code.to_string()))?;
        Ok(self.sot + 1 + index as u32)
    }

    /// The code a language token stands for.
    pub fn language_code(&self, token: u32) -> Option<&'static str> {
        self.language_tokens().contains(&token).then(|| LANGUAGES[(token - self.sot - 1) as usize].0)
    }

    pub fn is_timestamp(&self, token: u32) -> bool {
        token >= self.timestamp_begin
    }

    /// Seconds into the window a timestamp token marks.
    pub fn timestamp_seconds(&self, token: u32) -> f64 {
        token.saturating_sub(self.timestamp_begin) as f64 * TIMESTAMP_SECONDS
    }

    /// Tokens that are never sampled: the non-speech symbols and every control token that
    /// would restart or redirect the transcript mid-stream.
    pub fn suppressed(&self) -> Vec<u32> {
        let mut ids = NON_SPEECH.to_vec();
        ids.extend([self.sot, self.translate, self.transcribe, self.sot_lm, self.sot_prev, self.no_speech]);
        ids
    }

    /// Tokens barred from the first sampled position: a leading blank, or ending at once.
    pub fn suppressed_at_start(&self) -> [u32; 2] {
        [BLANK, self.eot]
    }

    /// Raw bytes of a text token; empty for specials. Tokens split UTF-8 sequences between
    /// them, so callers concatenate bytes before decoding.
    pub fn bytes(&self, token: u32) -> &'static [u8] {
        if token >= N_TEXT {
            return &[];
        }
        let (start, next) = (self.starts[token as usize] as usize, self.starts[token as usize + 1] as usize);
        &PACKED[start..next - 1]
    }

    /// Text of a token run, specials and timestamps dropped.
    pub fn decode(&self, tokens: &[u32]) -> String {
        let bytes: Vec<u8> = tokens.iter().flat_map(|&t| self.bytes(t).iter().copied()).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// One token for a debug trace: text tokens verbatim, specials by name.
    pub fn describe(&self, token: u32) -> String {
        if token < N_TEXT {
            return String::from_utf8_lossy(self.bytes(token)).into_owned();
        }
        if let Some(code) = self.language_code(token) {
            return format!("<|{code}|>");
        }
        if self.is_timestamp(token) {
            return format!("<|{:.2}|>", self.timestamp_seconds(token));
        }
        let name = match token {
            t if t == self.eot => "endoftext",
            t if t == self.sot => "startoftranscript",
            t if t == self.translate => "translate",
            t if t == self.transcribe => "transcribe",
            t if t == self.sot_lm => "startoflm",
            t if t == self.sot_prev => "startofprev",
            t if t == self.no_speech => "nospeech",
            t if t == self.no_timestamps => "notimestamps",
            _ => "?",
        };
        format!("<|{name}|>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ids below are the ones in openai/whisper-base and openai/whisper-large-v3-turbo's
    // generation_config.json.
    #[test]
    fn special_ids_match_the_99_language_models() {
        let v = Vocab::multilingual(51865).unwrap();
        assert_eq!((v.eot, v.sot), (50257, 50258));
        assert_eq!((v.translate, v.transcribe, v.sot_lm, v.sot_prev, v.no_speech), (50358, 50359, 50360, 50361, 50362));
        assert_eq!((v.no_timestamps, v.timestamp_begin), (50363, 50364));
        assert_eq!(v.language_tokens(), 50259..50358);
        assert!(v.language_token("yue").is_err());
    }

    #[test]
    fn special_ids_match_the_large_v3_family() {
        let v = Vocab::multilingual(51866).unwrap();
        assert_eq!((v.translate, v.transcribe, v.sot_lm, v.sot_prev, v.no_speech), (50359, 50360, 50361, 50362, 50363));
        assert_eq!((v.no_timestamps, v.timestamp_begin), (50364, 50365));
        assert_eq!(v.language_token("yue").unwrap(), 50358);
        // The last timestamp is the last id.
        assert_eq!(v.timestamp_begin + N_TIMESTAMPS, 51866);
    }

    #[test]
    fn language_ids_match_the_reference_table() {
        let v = Vocab::multilingual(51866).unwrap();
        for (code, id) in [("en", 50259), ("zh", 50260), ("ar", 50272), ("yo", 50325), ("af", 50327), ("am", 50334)] {
            assert_eq!(v.language_token(code).unwrap(), id, "{code}");
            assert_eq!(v.language_code(id), Some(code));
        }
        assert_eq!(v.language_code(v.transcribe), None);
    }

    #[test]
    fn suppressed_set_matches_the_generation_config() {
        let v = Vocab::multilingual(51866).unwrap();
        let ids = v.suppressed();
        assert_eq!(ids.len(), 88);
        assert_eq!(&ids[82..], &[50258, 50359, 50360, 50361, 50362, 50363]);
        let v = Vocab::multilingual(51865).unwrap();
        assert_eq!(&v.suppressed()[82..], &[50258, 50358, 50359, 50360, 50361, 50362]);
    }

    #[test]
    fn decodes_a_reference_transcript() {
        let v = Vocab::multilingual(51865).unwrap();
        // whisper-base's greedy output for tests/fixtures/jfk.s16, specials included.
        let ids = [
            50258, 50259, 50359, 50363, 400, 370, 452, 7177, 6280, 11, 1029, 406, 437, 428, 1941, 393, 360, 337, 291, 11, 1029, 437, 291,
            393, 360, 337, 428, 1941, 13, 50257,
        ];
        assert_eq!(v.decode(&ids), " And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country.");
        assert_eq!(v.describe(50359), "<|transcribe|>");
        assert_eq!(v.describe(400), " And");
    }

    #[test]
    fn multibyte_text_survives_token_boundaries() {
        let v = Vocab::multilingual(51865).unwrap();
        // "é" is one token here; a lone continuation byte must not panic either.
        let e_acute = (0..N_TEXT).find(|&t| v.bytes(t) == "é".as_bytes()).expect("é is in the vocabulary");
        assert_eq!(v.decode(&[e_acute]), "é");
        let lone = (0..N_TEXT).find(|&t| v.bytes(t) == [0xa9]).expect("byte tokens are in the vocabulary");
        assert_eq!(v.decode(&[lone]), "\u{fffd}");
    }

    #[test]
    fn timestamps_and_unknown_sizes() {
        let v = Vocab::multilingual(51866).unwrap();
        assert!(v.is_timestamp(v.timestamp_begin));
        assert!(!v.is_timestamp(v.no_timestamps));
        assert!((v.timestamp_seconds(v.timestamp_begin + 50) - 1.0).abs() < 1e-9);
        assert!(matches!(Vocab::multilingual(51864), Err(Error::UnknownVocab(51864))));
        assert!(matches!(Vocab::multilingual(1000), Err(Error::UnknownVocab(1000))));
    }
}
