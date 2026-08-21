//! Read facts out of a Docker Compose file at compile time: the name, tag and digest of a
//! service's image, and its environment variables.
//!
//! A `docker-compose.yaml` already declares which image version a project runs and how its
//! services are configured, and tooling like Dependabot keeps that declaration current. Restating
//! any of it in Rust creates a second source of truth that drifts silently. Reading it instead
//! leaves the compose file as the one place a version or a credential is written down.
//!
//! Start from the [`compose!`] macro, which embeds and parses the file:
//!
//! ```
//! use composed::{Compose, compose};
//! use std::sync::LazyLock;
//!
//! static COMPOSE: LazyLock<Compose> = LazyLock::new(|| compose!("docker-compose.yaml"));
//!
//! let postgres = COMPOSE.service("postgres");
//! assert_eq!(postgres.image().tag(), "18-alpine");
//! assert_eq!(postgres.env("POSTGRES_USER"), "composed");
//! ```
//!
//! The motivating case is starting containers from tests, e.g. with
//! [testcontainers](https://docs.rs/testcontainers), which otherwise hardcode an image tag and,
//! for images like PostgreSQL, a set of credentials that the local stack already declares.
//!
//! Lookups panic instead of returning an error. The input is a file the caller embedded at compile
//! time, so a compose file that does not declare what is asked of it is a mistake in the
//! repository rather than a condition worth handling. Each panic names the compose file and lists
//! what it does declare.

#![warn(missing_docs)]

use serde::{
    Deserialize, Deserializer,
    de::{self, Unexpected, Visitor},
};
use std::{
    collections::BTreeMap,
    fmt::{self, Formatter},
};

/// Embed and parse a Docker Compose file at compile time.
///
/// The path literal is resolved relative to the *calling* crate's `CARGO_MANIFEST_DIR`, so a crate
/// in a workspace subdirectory passes `"../docker-compose.yaml"` and a crate at the repository
/// root passes `"docker-compose.yaml"`. The file is embedded with [`include_str!`], so a missing or
/// renamed file is a compile error rather than a failure at run time.
///
/// Expands to a [`Compose`] value, leaving the call site to decide how to hold it. Prefer a module
/// level [`LazyLock`](std::sync::LazyLock), which parses once per process and hands out
/// `&'static str`:
///
/// ```
/// use composed::{Compose, compose};
/// use std::sync::LazyLock;
///
/// static COMPOSE: LazyLock<Compose> = LazyLock::new(|| compose!("docker-compose.yaml"));
///
/// assert_eq!(COMPOSE.image("postgres").name(), "postgres");
/// ```
///
/// # Panics
///
/// If the file is not valid YAML or does not describe a Compose file.
#[macro_export]
macro_rules! compose {
    ($path:literal) => {
        $crate::Compose::parse(
            concat!(env!("CARGO_MANIFEST_DIR"), "/", $path),
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $path)),
        )
    };
}

/// A parsed Docker Compose file.
///
/// Built by [`compose!`]. Lookups panic, naming the compose file and listing what it does declare.
#[derive(Debug, Clone)]
pub struct Compose {
    source: String,
    services: BTreeMap<String, Service>,
}

impl Compose {
    /// Parse the given Compose file contents, using `source`, the file path, in panic messages.
    ///
    /// Prefer [`compose!`], which fills in both arguments.
    ///
    /// # Panics
    ///
    /// If `yaml` is not valid YAML or has no `services` mapping.
    #[track_caller]
    pub fn parse(source: &str, yaml: &str) -> Self {
        let raw = match serde_norway::from_str::<RawCompose>(yaml) {
            Ok(raw) => raw,
            Err(error) => panic!("{source} is not a valid Compose file: {error}"),
        };

        let services = raw
            .services
            .into_iter()
            .map(|(name, service)| {
                let service = Service {
                    source: source.to_string(),
                    name: name.clone(),
                    image: service.image,
                    environment: service.environment.map(Into::into).unwrap_or_default(),
                };

                (name, service)
            })
            .collect();

        Self {
            source: source.to_string(),
            services,
        }
    }

    /// The image of the service with the given name, i.e. [`Compose::service`] followed by
    /// [`Service::image`].
    ///
    /// # Panics
    ///
    /// If there is no such service or it declares no image.
    #[track_caller]
    pub fn image(&self, service: &str) -> Image<'_> {
        self.service(service).image()
    }

    /// The service with the given name.
    ///
    /// A commented out service block does not exist as far as YAML is concerned and is therefore
    /// not found.
    ///
    /// # Panics
    ///
    /// If there is no such service.
    #[track_caller]
    pub fn service(&self, name: &str) -> &Service {
        match self.services.get(name) {
            Some(service) => service,

            None => panic!(
                "no service `{name}` in {}; known services: [{}]",
                self.source,
                self.service_names().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The names of all declared services, sorted.
    pub fn service_names(&self) -> impl Iterator<Item = &str> {
        self.services.keys().map(String::as_str)
    }
}

/// A service of a [`Compose`] file.
///
/// Exposes the image and the environment, and deliberately not `ports` or `command`: a caller that
/// starts the service itself publishes its own ports and may need a different command than the
/// local stack.
#[derive(Debug, Clone)]
pub struct Service {
    source: String,
    name: String,
    image: Option<String>,
    environment: BTreeMap<String, Option<String>>,
}

impl Service {
    /// The name of this service.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The image of this service, split into name, tag and digest.
    ///
    /// # Panics
    ///
    /// If this service declares no image, e.g. because it is built rather than pulled.
    #[track_caller]
    pub fn image(&self) -> Image<'_> {
        let reference = match self.image.as_deref() {
            Some(reference) => reference,

            None => panic!(
                "service `{}` in {} declares no image",
                self.name, self.source
            ),
        };

        Image::parse(reference, &self.name, &self.source)
    }

    /// The value of the given environment variable of this service.
    ///
    /// Both the mapping form (`POSTGRES_USER: postgres`) and the list form
    /// (`- POSTGRES_USER=postgres`) are understood.
    ///
    /// A value that YAML reads as a number or a boolean is rendered from the parsed value, not
    /// from the text as written, so `PGPORT: 5432` yields `"5432"` but `1.50` yields `"1.5"` and
    /// `1e3` yields `"1000"`. Quote any value that has to survive verbatim.
    ///
    /// # Panics
    ///
    /// If this service has no such environment variable, or declares it without a value, which
    /// passes the host's value through and so cannot be read statically.
    #[track_caller]
    pub fn env(&self, key: &str) -> &str {
        match self.environment.get(key) {
            Some(Some(value)) => value,

            Some(None) => panic!(
                "environment variable `{key}` of service `{}` in {} has no value; a value passed \
                 through from the host cannot be read statically",
                self.name, self.source
            ),

            None => panic!(
                "no environment variable `{key}` for service `{}` in {}; known variables: [{}]",
                self.name,
                self.source,
                self.env_keys().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The names of all environment variables of this service, sorted.
    pub fn env_keys(&self) -> impl Iterator<Item = &str> {
        self.environment.keys().map(String::as_str)
    }
}

/// The image of a [`Service`], e.g. `ghcr.io/umadb-io/umadb:0.7`.
///
/// Borrows from the [`Compose`] it came from, not from the accessor's temporary, so
/// `COMPOSE.image("postgres").tag()` needs no intermediate binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image<'a> {
    reference: &'a str,
    name: &'a str,
    tag: Option<&'a str>,
    digest: Option<&'a str>,
    service: &'a str,
    source: &'a str,
}

impl<'a> Image<'a> {
    /// The image reference exactly as written in the compose file.
    pub fn reference(&self) -> &'a str {
        self.reference
    }

    /// [`Image::name`] and [`Image::tag`] as a pair, for constructors taking both as one type
    /// parameter, such as testcontainers' `GenericImage::new`.
    ///
    /// # Panics
    ///
    /// If the reference has no tag.
    #[track_caller]
    pub fn name_and_tag(&self) -> (&'a str, &'a str) {
        (self.name(), self.tag())
    }

    /// The repository part of the reference, including any registry and namespace and excluding
    /// tag and digest.
    ///
    /// A registry port is not mistaken for a tag: `localhost:5000/image` has the name
    /// `localhost:5000/image` and no tag.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The tag part of the reference.
    ///
    /// # Panics
    ///
    /// If the reference has no tag, i.e. is unpinned or pinned by digest only.
    #[track_caller]
    pub fn tag(&self) -> &'a str {
        match self.tag {
            Some(tag) => tag,

            None => panic!(
                "image `{}` of service `{}` in {} has no tag",
                self.reference, self.service, self.source
            ),
        }
    }

    /// The digest part of the reference, e.g. `sha256:...`, if it is pinned by digest.
    pub fn digest(&self) -> Option<&'a str> {
        self.digest
    }

    fn parse(reference: &'a str, service: &'a str, source: &'a str) -> Self {
        let (rest, digest) = match reference.split_once('@') {
            Some((rest, digest)) => (rest, Some(digest)),
            None => (reference, None),
        };

        let segment = rest.rfind('/').map_or(0, |index| index + 1);
        let (name, tag) = match rest[segment..].rfind(':') {
            Some(index) => {
                let index = segment + index;
                (&rest[..index], Some(&rest[index + 1..]))
            }

            None => (rest, None),
        };

        Self {
            reference,
            name,
            tag: tag.filter(|tag| !tag.is_empty()),
            digest: digest.filter(|digest| !digest.is_empty()),
            service,
            source,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawCompose {
    services: BTreeMap<String, RawService>,
}

#[derive(Debug, Deserialize)]
struct RawService {
    #[serde(default)]
    image: Option<String>,

    #[serde(default)]
    environment: Option<RawEnvironment>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawEnvironment {
    Map(BTreeMap<String, Option<Scalar>>),
    List(Vec<String>),
}

impl From<RawEnvironment> for BTreeMap<String, Option<String>> {
    fn from(environment: RawEnvironment) -> Self {
        match environment {
            RawEnvironment::Map(map) => map
                .into_iter()
                .map(|(key, value)| (key, value.map(|Scalar(value)| value)))
                .collect(),

            RawEnvironment::List(list) => list
                .into_iter()
                .map(|entry| match entry.split_once('=') {
                    Some((key, value)) => (key.to_string(), Some(value.to_string())),
                    None => (entry, None),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ScalarVisitor;

        impl Visitor<'_> for ScalarVisitor {
            type Value = Scalar;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("a string, integer, float or boolean")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Scalar(value.to_string()))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Scalar(value.to_string()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Scalar(value.to_string()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Scalar(value.to_string()))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Scalar(value.to_string()))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::invalid_type(Unexpected::Unit, &self))
            }
        }

        deserializer.deserialize_any(ScalarVisitor)
    }
}

#[cfg(test)]
mod tests {
    use crate::Compose;

    const SOURCE: &str = "test.yaml";

    #[test]
    fn compose_macro_reads_this_crate_s_file() {
        let compose = compose!("docker-compose.yaml");
        let postgres = compose.service("postgres");

        assert_eq!(postgres.image().name(), "postgres");
        assert_eq!(postgres.image().tag(), "18-alpine");
        assert_eq!(postgres.env("POSTGRES_USER"), "composed");
    }

    #[test]
    fn quoted_and_unquoted_images() {
        let compose = Compose::parse(
            SOURCE,
            r#"
services:
  quoted:
    image: "postgres:18-alpine"
  unquoted:
    image: ghcr.io/umadb-io/umadb:0.7
"#,
        );

        assert_eq!(compose.image("quoted").name(), "postgres");
        assert_eq!(compose.image("quoted").tag(), "18-alpine");
        assert_eq!(compose.image("unquoted").name(), "ghcr.io/umadb-io/umadb");
        assert_eq!(compose.image("unquoted").tag(), "0.7");
    }

    #[test]
    fn registry_port_is_not_a_tag() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: localhost:5000/image\n");
        let image = compose.image("s");

        assert_eq!(image.name(), "localhost:5000/image");
        assert_eq!(image.digest(), None);
    }

    #[test]
    #[should_panic(expected = "has no tag")]
    fn registry_port_only_has_no_tag() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: localhost:5000/image\n");
        compose.image("s").tag();
    }

    #[test]
    fn registry_port_with_tag() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: localhost:5000/image:1.2\n",
        );
        let image = compose.image("s");

        assert_eq!(image.name(), "localhost:5000/image");
        assert_eq!(image.tag(), "1.2");
    }

    #[test]
    fn digest_without_tag() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: image@sha256:abc\n");
        let image = compose.image("s");

        assert_eq!(image.name(), "image");
        assert_eq!(image.digest(), Some("sha256:abc"));
        assert_eq!(image.reference(), "image@sha256:abc");
    }

    #[test]
    #[should_panic(expected = "has no tag")]
    fn digest_without_tag_has_no_tag() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: image@sha256:abc\n");
        compose.image("s").tag();
    }

    #[test]
    fn tag_and_digest() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: image:1.2@sha256:abc\n");
        let image = compose.image("s");

        assert_eq!(image.name(), "image");
        assert_eq!(image.tag(), "1.2");
        assert_eq!(image.digest(), Some("sha256:abc"));
        assert_eq!(image.name_and_tag(), ("image", "1.2"));
    }

    #[test]
    fn environment_list_form() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      - A=1\n      - B=two\n",
        );
        let service = compose.service("s");

        assert_eq!(service.env("A"), "1");
        assert_eq!(service.env("B"), "two");
        assert_eq!(service.env_keys().collect::<Vec<_>>(), ["A", "B"]);
    }

    #[test]
    fn non_string_scalars() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      PGPORT: 5432\n      DEBUG: true\n",
        );
        let service = compose.service("s");

        assert_eq!(service.env("PGPORT"), "5432");
        assert_eq!(service.env("DEBUG"), "true");
    }

    #[test]
    fn numeric_scalars_do_not_survive_verbatim() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      A: 1.50\n      B: \"1.50\"\n",
        );
        let service = compose.service("s");

        assert_eq!(service.env("A"), "1.5");
        assert_eq!(service.env("B"), "1.50");
    }

    #[test]
    fn boolean_lookalikes_stay_strings() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      A: NO\n      B: yes\n      C: off\n",
        );
        let service = compose.service("s");

        assert_eq!(service.env("A"), "NO");
        assert_eq!(service.env("B"), "yes");
        assert_eq!(service.env("C"), "off");
    }

    #[test]
    fn service_without_environment() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    image: i:1\n");

        assert_eq!(compose.service("s").env_keys().count(), 0);
        assert_eq!(compose.service("s").name(), "s");
    }

    #[test]
    fn comments_and_other_top_level_keys_are_ignored() {
        let compose = Compose::parse(
            SOURCE,
            r#"
services:
  postgres:
    image: postgres:18-alpine
    volumes:
      - "pgdata:/var/lib/postgresql"

  # otel-collector:
  #   image: otel/opentelemetry-collector-contrib:0.122.1

volumes:
  pgdata:
"#,
        );

        assert_eq!(compose.service_names().collect::<Vec<_>>(), ["postgres"]);
    }

    #[test]
    #[should_panic(expected = "known services: [a, b]")]
    fn unknown_service_lists_known_services() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  a:\n    image: i:1\n  b:\n    image: i:1\n",
        );
        compose.service("c");
    }

    #[test]
    #[should_panic(expected = "known variables: [A, B]")]
    fn unknown_environment_variable_lists_known_variables() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      A: 1\n      B: 2\n",
        );
        compose.service("s").env("C");
    }

    #[test]
    #[should_panic(expected = "cannot be read statically")]
    fn environment_variable_without_value() {
        let compose = Compose::parse(
            SOURCE,
            "services:\n  s:\n    image: i:1\n    environment:\n      A:\n",
        );
        compose.service("s").env("A");
    }

    #[test]
    #[should_panic(expected = "declares no image")]
    fn service_without_image() {
        let compose = Compose::parse(SOURCE, "services:\n  s:\n    build: .\n");
        compose.image("s");
    }

    #[test]
    #[should_panic(expected = "is not a valid Compose file")]
    fn invalid_compose_file() {
        Compose::parse(SOURCE, "not: a compose file\n");
    }
}
