//! Secret masking for free text, applied to everything the server stores.
//!
//! [`redact_text`] masks, in order:
//!
//! 1. **Credential formats** anywhere in the text (`sk-`, `sk-ant-`, `ghp_`/`gho_`/`ghs_`/`ghu_`,
//!    `github_pat_`, `xox?-`, `AKIA…`, `AIza…`, `glpat-`, `hf_`, `npm_`, `sk_live_`/`sk_test_`/`rk_live_`, JWTs, `Bearer `/`Basic `
//!    credentials, PEM blocks).
//! 2. **Assignments** `NAME=value` / `NAME: value` whose name looks secret (`api_key`, `token`,
//!    `secret`, `password`, `passwd`, `credential`): the value only.
//! 3. **URL secrets**: userinfo becomes `••••` (`https://••••@host`) and secret-looking query
//!    values are masked.
//! 4. **Entropy**: whitespace-separated words that look random (see `rules::is_high_entropy`).
//!    UUIDs, paths, URLs and word-like identifiers are exempt.
//!
//! [`has_credentials`] only asks whether text holds a credential format or a URL with a literal
//! password or secret query value, for script bodies that are stored as written.

mod rules;

use rules::{MASK, has_token, has_token_in_name, has_url_credentials, is_high_entropy, mask_formats, mask_tokens};

/// Masks secrets in free text (note titles, bodies, tags): credential formats, `NAME=value`
/// assignments with secret-looking names, URL userinfo and secret query values, and
/// whitespace-separated words that pass the entropy heuristic.
pub fn redact_text(text: &str) -> String {
    let masked = mask_tokens(text);

    if !masked.split_whitespace().any(is_high_entropy) {
        return masked;
    }

    let mut out = String::with_capacity(masked.len());
    let mut rest = masked.as_str();

    // Rebuild word by word so the original whitespace (including newlines) is preserved.
    while let Some(start) = rest.find(|character: char| !character.is_whitespace()) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        out.push_str(if is_high_entropy(word) { MASK } else { word });
        rest = &rest[end..];
    }

    out.push_str(rest);
    out
}

/// Masks script output: credential formats, secret assignments and URL secrets, as
/// [`redact_text`] does, but not the entropy rule, which would mask compact JSON words.
pub fn redact_output(text: &str) -> String {
    mask_tokens(text)
}

/// Masks credential formats and URL secrets only, for text that is code: a name like `token: String` or
/// `API_KEY = x` is how code is written, and masking the value after it would hide the type, not a secret.
pub fn redact_formats(text: &str) -> String {
    mask_formats(text)
}

/// True when `text` holds a credential format (as [`redact_text`] masks them; a PEM block only
/// when it is a private key) or a URL with a literal password or secret query value. Variable
/// references (`$GH_TOKEN`, `https://u:${PASS}@host`), `NAME=value` assignments and random-looking
/// words do not count: a script reads its secrets from the environment.
pub fn has_credentials(text: &str) -> bool {
    has_token(text) || has_url_credentials(text)
}

/// Formats that start with a prefix no ordinary word ends in (`ghp_`, `glpat-`, `AKIA`): a name may have glued one
/// to the word before it (`.pghp_x`), so these are tried from every place they are written.
const GLUEABLE_STARTS: &[&str] = &[
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "AIza",
    "glpat-",
    "npm_",
    "eyJ",
    "-----BEGIN ",
];

/// Formats whose prefix ends ordinary words (`task-`, `disk-`, `risk_live_`, `pdf_`): trying them from every place
/// they are written would drop `.task-list-item-checkbox`, so they are tried only after an underscore (`.a_sk-x`),
/// which gives the plain check no word boundary; at a name start or after `-` or `.` it already finds them.
const SEPARATED_STARTS: &[&str] = &["sk-", "hf_", "sk_live_", "sk_test_", "rk_live_", "rk_test_"];

/// The suffixes of a name that are tried apart from it: a name of the length an index keeps holds fewer.
const MAX_GLUED_STARTS: usize = 64;

/// [`has_credentials`] for a name that is made of parts (a selector built by nesting, `.p` and `&ghp_x`, or a
/// class written after an underscore): a credential format is matched at a word boundary, and a name has glued
/// it to the word before it (`.pghp_x`, `.a_ghp_x`), where no boundary is. A format with a distinctive prefix is
/// therefore tried from every place it is written; one whose prefix ends ordinary words (`sk-` in `task-`) only
/// after an underscore, so that `.task-list-item-checkbox` and `.disk-usage-warning-panel` are no credentials.
/// A bare `sk-` key counts only with a tail that looks random (a digit, an upper and a lower case letter), so
/// `.sk-circle-loader-wrapper-outer` is a selector. The name is tried again without its whitespace, which a
/// comment inside a name (`.ghp_/**/…`) leaves behind. A text with more starts than [`MAX_GLUED_STARTS`] is no
/// name; it counts as a credential, so the work stays bounded.
pub fn has_credentials_in_name(text: &str) -> bool {
    if name_holds_credential(text) {
        return true;
    }

    if !text.contains(char::is_whitespace) {
        return false;
    }

    let joined: String = text.chars().filter(|character| !character.is_whitespace()).collect();

    name_holds_credential(&joined)
}

/// One pass of [`has_credentials_in_name`] over a text as it is.
fn name_holds_credential(text: &str) -> bool {
    if has_token_in_name(text) || has_url_credentials(text) {
        return true;
    }

    let bytes = text.as_bytes();
    let glued = GLUEABLE_STARTS
        .iter()
        .flat_map(|start| text.match_indices(start).map(|(at, _)| at));
    let separated = SEPARATED_STARTS.iter().flat_map(|start| {
        text.match_indices(start)
            .map(|(at, _)| at)
            .filter(|at| at.checked_sub(1).and_then(|before| bytes.get(before)) == Some(&b'_'))
    });

    let mut starts: Vec<usize> = glued.chain(separated).collect();

    starts.sort_unstable();
    starts.dedup();

    if starts.len() > MAX_GLUED_STARTS {
        return true;
    }

    starts.into_iter().any(|at| {
        let Some(rest) = text.get(at..) else {
            return false;
        };

        has_token_in_name(rest) || is_glued_aws_key(rest)
    })
}

/// True when `text` starts with an AWS access key id that something is written right after (`AKIA…_x`): the
/// format asks for a boundary after it, and a name has none.
fn is_glued_aws_key(text: &str) -> bool {
    text.strip_prefix("AKIA")
        .and_then(|rest| rest.as_bytes().get(..16))
        .is_some_and(|id| id.iter().all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn code_keeps_its_assignments_while_credential_formats_and_url_secrets_are_masked() {
        let token = format!("gh{}_{}", "p", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4");

        assert_eq!(
            redact_formats("pub token: String, password: &str"),
            "pub token: String, password: &str"
        );
        assert_eq!(redact_formats(&format!("fn {token}()")), "fn ••••()");
        assert_eq!(
            redact_formats("let u = https://user:pa55@host/p?sig=abc"),
            "let u = https://••••@host/p?sig=••••"
        );
        // the assignment rule of the free-text scrubber is what this one leaves out
        assert_eq!(redact_output("API_KEY = hunter2"), "API_KEY = ••••");
        assert_eq!(redact_formats("API_KEY = hunter2"), "API_KEY = hunter2");
    }

    #[test]
    fn credential_formats_are_masked_anywhere() {
        let samples = [
            "sk-proj-abcdefghijklmnop1234",
            "sk-ant-api03-abcdefghijklmnopqrstu",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "gho_abcdefghijklmnopqrstuvwxyz0123",
            "ghs_abcdefghijklmnopqrstuvwxyz0123",
            "ghu_abcdefghijklmnopqrstuvwxyz0123",
            "github_pat_11ABCDEFG0123456789_abcdef",
            "xoxb-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA-abcdefghijklmnopqrstuvwxyz01234",
            "glpat-abcdefghijklmnopqrst",
            "hf_abcdefghijklmnopqrstuvwxyz",
            "npm_abcdefghijklmnopqrstuvwxyz0123",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.signaturepart",
            "Bearer abcdefgh12345678",
            "Basic dXNlcjpwYXNzd29yZA==",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
            "prefix text with sk-ant-abcdefghijklmnopqrstuvwx inside",
        ];

        for sample in samples {
            assert!(redact_text(sample).contains(MASK), "{sample}");
        }
    }

    #[test]
    fn stripe_style_keys_are_masked_in_text_and_in_code() {
        let tail = "4eC39HqLyjWDarjtT1zdp7dc";

        for prefix in ["sk_live_", "sk_test_", "rk_live_"] {
            let key = [prefix, tail].concat();

            assert!(has_credentials(&key), "{prefix}");
            assert_eq!(redact_formats(&format!("let k = {key};")), "let k = ••••;", "{prefix}");
        }

        // too short to be a key, and a name that only starts like one
        assert!(!has_credentials(&["sk_live_", "short"].concat()));
        assert!(!has_credentials("risk_live_values_of_the_market"));
    }

    #[test]
    fn a_credential_glued_to_a_word_of_a_name_is_found_where_a_boundary_is_not() {
        let tail = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4";
        let token = ["gh", "p_", tail].concat();

        for glued in [
            format!(".p{token}"),
            format!(".a_{token}"),
            format!(".x-{token}"),
            format!("a{token}-hover"),
        ] {
            assert!(has_credentials_in_name(&glued), "{glued}");
        }

        // the plain check keeps its meaning for every other caller
        assert!(!has_credentials(&format!(".p{token}")));
        assert!(has_credentials_in_name(&token));

        // names that only look like a start
        for fine in [".ghp_short", ".sk-circle", ".BasicButton", ".github_pat", ".npm_run"] {
            assert!(!has_credentials_in_name(fine), "{fine}");
        }
    }

    #[test]
    fn real_names_that_end_in_a_credential_prefix_are_no_credentials() {
        let names = [
            ".task-list-item-checkbox",
            ".disk-usage-warning-panel",
            ".mask-image-container-wrapper",
            "$mask-image-container-wrapper",
            "--task-list-item-checkbox-bg",
            "disk-usage-warning-pulse",
            ".flask-application-wrapper-x",
            ".desk-reservation-calendar-grid",
            ".risk-assessment-summary-table",
            ".ask-the-community-widget-box",
            ".sk-circle-loader-wrapper-outer",
            ".token",
            ".password-field",
            ".api-key",
            ".auth-card",
            ".secret-banner",
            ".key-hint",
            ".card__title--highlighted-state",
            ".block__element_modifier-value-long",
            ".hover\\:bg-blue-500",
            ".w-1\\/2.md\\:flex.dark\\:text-gray-100",
            ".risk_live_values_of_the_market",
            ".pdf_viewer_container_wrapper_x",
        ];

        for name in names {
            assert!(!has_credentials_in_name(name), "{name}");
        }
    }

    #[test]
    fn synthetic_credentials_in_a_name_are_found_however_they_are_glued() {
        let tail = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        let aws = ["AKIA", "ABCDEFGHIJKLMNOP"].concat();
        let found = [
            [".ghp_", tail].concat(),
            [".task-ghp_", tail].concat(),
            [".gho_", tail].concat(),
            [".ghu_", tail].concat(),
            [".ghs_", tail].concat(),
            [".github_pat_", tail].concat(),
            [".glpat-", tail].concat(),
            [".xoxb-", "1234567890-abcdefghij"].concat(),
            [".npm_", tail].concat(),
            [".sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"].concat(),
            [".a_sk_test_", "4eC39HqLyjWDarjtT1zdp7dc"].concat(),
            [".sk-", tail].concat(),
            ["sk-ant-", tail].concat(),
            [".a_sk-", tail].concat(),
            [".disk-sk-", tail].concat(),
            format!(".{aws}"),
            format!(".{aws}_x"),
            format!(".{aws}x"),
            format!(".pre{aws}"),
            // a comment inside the name leaves whitespace between its parts
            [".ghp_ ", tail].concat(),
            [".ghp_\t\n", tail].concat(),
        ];

        for name in found {
            assert!(has_credentials_in_name(&name), "{name}");
        }
    }

    #[test]
    fn url_userinfo_and_secret_query_values_are_masked_but_the_url_stays_readable() {
        assert_eq!(
            redact_text("https://user:pa55@proxy.local:3128/v1"),
            "https://••••@proxy.local:3128/v1"
        );
        assert_eq!(
            redact_text("see https://mcp.example.com/sse?sig=abc123&page=2 now"),
            "see https://mcp.example.com/sse?sig=••••&page=2 now"
        );
    }

    #[test]
    fn high_entropy_words_are_masked() {
        for sample in [
            concat!("aB3dE5fG7hJ9kL1mN3pQ5", "rS7tU9vW1xY3zA5bC7d"),
            "Xk9#mP2$vL8@nQ4!wR6^tY0&uI3*oE5(",
        ] {
            assert_eq!(redact_text(sample), MASK, "{sample}");
        }
    }

    #[test]
    fn ordinary_long_values_stay_visible() {
        let visible = [
            "This is a perfectly normal sentence about configuration.",
            "/Users/Frogius/Projekty/harness-useful-desktop2/src",
            "C:\\Users\\Frogius\\AppData\\Roaming\\npm\\node_modules",
            "https://github.com/MSpiechowicz/harness-useful-desktop",
            // UUIDs are identifiers (session ids), not credentials: shown.
            "7218123d-0746-47cd-b886-473b29bc5f49",
            "anthropic/claude-3-5-sonnet-20241022-v2:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "openrouter/meta-llama/llama-3.1-405b-instruct",
        ];

        for sample in visible {
            assert_eq!(redact_text(sample), sample, "{sample}");
        }
    }

    #[test]
    fn redact_text_masks_tokens_assignments_userinfo_and_random_words() {
        let input = concat!(
            "token sk-ant-abcdefghijklmnopqrstuv used\n\
                     GITHUB_TOKEN=abc123 password: hunter2\n\
                     clone https://me:pw@host/x.git id 7218123d-0746-47cd-b886-473b29bc5f49\n\
                     blob aB3dE5fG7hJ9kL1mN3pQ5",
            "rS7tU9vW1xY3zA5bC7d end"
        );

        let expected = "token •••• used\n\
                        GITHUB_TOKEN=•••• password: ••••\n\
                        clone https://••••@host/x.git id 7218123d-0746-47cd-b886-473b29bc5f49\n\
                        blob •••• end";

        assert_eq!(redact_text(input), expected);
    }

    #[test]
    fn redact_text_leaves_plain_text_untouched() {
        let input = "Compiling harness_memory v0.1.0\n  Finished in 1.2s";

        assert_eq!(redact_text(input), input);
    }

    #[test]
    fn credentials_in_script_bodies_are_found() {
        let found = [
            format!(
                "curl -H 'Authorization: token {}' https://api.github.com",
                "ghp_".to_owned() + &"a".repeat(36)
            ),
            concat!("export KEY=AKIA", "ABCDEFGHIJKLMNOP").to_owned(),
            "curl -H \"Authorization: Bearer abcdefgh12345678\" $URL".to_owned(),
            "git clone https://me:pa55word@example.com/x.git".to_owned(),
            "fetch('https://api.example.com/v1?api_key=abc123&page=2')".to_owned(),
            "curl \"https://hooks.example.com/run?sig=deadbeef\"".to_owned(),
            "cat <<KEY\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\nKEY".to_owned(),
        ];

        for text in found {
            assert!(has_credentials(&text), "{text}");
        }
    }

    #[test]
    fn variable_references_and_plain_scripts_are_not_credentials() {
        let clean = [
            "#!/usr/bin/env bash\nset -euo pipefail\ngh pr list --json number,title",
            "curl -H \"Authorization: Bearer $GH_TOKEN\" https://api.github.com/user",
            "git clone https://x-access-token:${GH_TOKEN}@github.com/o/r.git",
            "git clone https://user:$PASSWORD@example.com/x.git",
            "git clone ssh://git@github.com/o/r.git",
            "curl \"https://api.example.com/v1?api_key=$API_KEY&page=2\"",
            "GH_TOKEN=$(gh auth token) gh api /user",
            "password = os.environ['DB_PASSWORD']",
            concat!("echo aB3dE5fG7hJ9kL1mN3pQ5", "rS7tU9vW1xY3zA5bC7d"),
            "openssl x509 -in cert.pem | grep -q -- '-----BEGIN CERTIFICATE-----'",
        ];

        for text in clean {
            assert!(!has_credentials(text), "{text}");
        }
    }

    #[test]
    fn script_output_keeps_compact_json_and_masks_credentials() {
        let json = r#"[{"number":18,"title":"feat(extensions): update plugins"},{"number":6,"title":"docs:"}]"#;
        assert_eq!(redact_output(json), json);
        assert_ne!(
            redact_text(json),
            json,
            "the entropy rule is what redact_output leaves out"
        );

        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4).get(..36).expect("36"));
        assert!(!redact_output(&format!("token {token}")).contains(&token));
    }
}
