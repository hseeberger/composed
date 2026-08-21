# composed

[![Crates.io][crates-badge]][crates-url]
[![docs.rs][docs-badge]][docs-url]
[![license][license-badge]][license-url]
[![build][build-badge]][build-url]

[crates-badge]: https://img.shields.io/crates/v/composed
[crates-url]: https://crates.io/crates/composed
[docs-badge]: https://img.shields.io/docsrs/composed
[docs-url]: https://docs.rs/composed
[license-badge]: https://img.shields.io/github/license/hseeberger/composed
[license-url]: https://github.com/hseeberger/composed/blob/main/LICENSE
[build-badge]: https://img.shields.io/github/actions/workflow/status/hseeberger/composed/ci.yml
[build-url]: https://github.com/hseeberger/composed/actions/workflows/ci.yml

Read facts out of a Docker Compose file at compile time: the name, tag and digest of a service's
image, and its environment variables.

A `docker-compose.yaml` already declares which image version a project runs and how its services
are configured, and tooling like Dependabot keeps that declaration current. Restating any of it in
Rust creates a second source of truth that drifts silently. Reading it instead leaves the compose
file as the one place a version or a credential is written down.

The motivating case is starting containers from tests, e.g. with
[testcontainers](https://docs.rs/testcontainers), which otherwise hardcode an image tag and, for
images like PostgreSQL, a set of credentials that the local stack already declares.

```rust
use composed::{Compose, compose};
use std::sync::LazyLock;

static COMPOSE: LazyLock<Compose> = LazyLock::new(|| compose!("docker-compose.yaml"));

let postgres = COMPOSE.service("postgres");

let container = Postgres::default()
    .with_db_name(postgres.env("POSTGRES_DB"))
    .with_user(postgres.env("POSTGRES_USER"))
    .with_password(postgres.env("POSTGRES_PASSWORD"))
    .with_tag(postgres.image().tag())
    .start()
    .await?;
```

The path given to `compose!` is relative to the *calling* crate's `CARGO_MANIFEST_DIR`, so a crate
in a workspace subdirectory passes `"../docker-compose.yaml"`. The file is embedded with
`include_str!`, so a missing or renamed file is a compile error. Lookups panic, naming the compose
file and listing what it does declare, like a failed assertion.

`ports` and `command` are deliberately not exposed: a caller that starts the service itself
publishes its own ports and may need a different command than the local stack.

A value that YAML reads as a number or a boolean is rendered from the parsed value rather than the
text as written, so `PGPORT: 5432` yields `"5432"` but `1.50` yields `"1.5"`. Quote any value that
has to survive verbatim.

## License ##

This code is open source software licensed under the [Apache 2.0 License](http://www.apache.org/licenses/LICENSE-2.0.html).
