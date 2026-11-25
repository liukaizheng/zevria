//! Input validation shared by the local search tool and hosted backend.

pub fn validate_search_domains(domains: &[String], field: &str) -> anyhow::Result<()> {
    anyhow::ensure!(domains.len() <= 100, "{field} permits at most 100 domains");
    for (index, domain) in domains.iter().enumerate() {
        // A DNS name, not a URL, wildcard, IP, port, or path. An all-numeric
        // final label also admits abbreviated IPv4 spellings in URL parsers.
        let valid = domain.len() <= 253
            && domain.contains('.')
            && domain.rsplit('.').next().is_some_and(|label| {
                label.bytes().any(|b| b.is_ascii_alphabetic())
                    && !label
                        .to_ascii_lowercase()
                        .strip_prefix("0x")
                        .is_some_and(|hex| {
                            !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
                        })
            })
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        anyhow::ensure!(
            valid,
            "{field}[{index}] must be a DNS domain without a scheme, port, wildcard, or path"
        );
    }
    Ok(())
}
