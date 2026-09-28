//! Endpoint-only support for integration tests of the real install pipeline.
//! No authenticated source, publication receipt, or runtime result is injected.

#[derive(Clone, Debug)]
pub struct InstallTestEndpoints {
    version_manifest: String,
    runtime_catalog: String,
    asset_objects: String,
}

impl InstallTestEndpoints {
    /// All fixture entrypoints belong to one literal loopback HTTP origin.
    /// This API is absent unless the explicit `test-support` feature is enabled.
    pub fn from_loopback_base_url(base_url: &str) -> Result<Self, String> {
        let url = reqwest::Url::parse(base_url).map_err(|error| error.to_string())?;
        let loopback = url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        });
        if url.scheme() != "http"
            || !loopback
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || url.port_or_known_default() == Some(0)
        {
            return Err("install fixtures require a literal loopback HTTP origin".to_owned());
        }
        Ok(Self {
            version_manifest: url.join("version_manifest_v2.json").unwrap().into(),
            runtime_catalog: url.join("java-runtime/all.json").unwrap().into(),
            asset_objects: url.join("assets/objects").unwrap().into(),
        })
    }

    pub(crate) fn version_manifest(&self) -> &str {
        &self.version_manifest
    }

    pub(crate) fn runtime_catalog(&self) -> &str {
        &self.runtime_catalog
    }

    pub(crate) fn asset_objects(&self) -> &str {
        &self.asset_objects
    }
}

#[cfg(test)]
mod tests {
    use super::InstallTestEndpoints;

    #[test]
    fn endpoint_support_accepts_only_literal_loopback_origins() {
        for base in ["http://127.0.0.1:12345", "http://[::1]:12345/"] {
            let endpoints = InstallTestEndpoints::from_loopback_base_url(base).unwrap();
            assert!(
                endpoints
                    .version_manifest()
                    .ends_with("/version_manifest_v2.json")
            );
            assert!(
                endpoints
                    .runtime_catalog()
                    .ends_with("/java-runtime/all.json")
            );
            assert!(endpoints.asset_objects().ends_with("/assets/objects"));
        }
        for base in [
            "https://127.0.0.1:12345",
            "http://localhost:12345",
            "http://192.0.2.1:12345",
            "http://127.0.0.1:0",
            "http://user:pass@127.0.0.1:12345",
            "http://127.0.0.1:12345/path",
            "http://127.0.0.1:12345/?query",
            "http://127.0.0.1:12345/#fragment",
        ] {
            assert!(
                InstallTestEndpoints::from_loopback_base_url(base).is_err(),
                "{base}"
            );
        }
    }
}
