use std::time::Duration;

/// Server under test.
#[derive(Debug, Clone, clap::Args)]
pub struct Target {
    /// DICOMweb root, e.g. `http://localhost:8042/dicom-web`.
    #[arg(long)]
    url: String,
    /// WADO-URI endpoint; defaults to `/wado` at the root of `--url`'s origin.
    #[arg(long)]
    wado_url: Option<String>,
    /// Basic authentication as `user:password`.
    #[arg(long)]
    user: Option<String>,
}

impl Target {
    pub fn base(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    pub fn wado(&self) -> String {
        self.wado_url.clone().unwrap_or_else(|| {
            let url = self.base();
            let host_start = url.find("://").map_or(0, |i| i + 3);
            let origin_end = url[host_start..]
                .find('/')
                .map_or(url.len(), |i| host_start + i);
            format!("{}/wado", &url[..origin_end])
        })
    }

    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self
            .user
            .as_deref()
            .map(|u| u.split_once(':').unwrap_or((u, "")))
        {
            Some((user, password)) => request.basic_auth(user, Some(password)),
            None => request,
        }
    }
}

pub fn client(concurrency: usize) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(600))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Target {
        Target {
            url: url.to_owned(),
            wado_url: None,
            user: None,
        }
    }

    #[test]
    fn wado_defaults_to_origin_root() {
        assert_eq!(
            target("http://h:8042/dicom-web/").wado(),
            "http://h:8042/wado"
        );
        assert_eq!(target("http://h:8042").wado(), "http://h:8042/wado");
    }
}
