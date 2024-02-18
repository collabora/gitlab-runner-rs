# gitlab-runner-rs

[![Crates.io][crates-badge]][crates-url]
[![Documentation][docs-badge]][docs-url]
[![MIT/Apache-2 licensed][license-badge]][license-url]
[![Build Status][actions-badge]][actions-url]

[crates-badge]: https://img.shields.io/crates/v/gitlab-runner
[crates-url]: https://crates.io/crates/gitlab-runner
[docs-badge]: https://docs.rs/gitlab-runner/badge.svg
[docs-url]: https://docs.rs/gitlab-runner
[license-badge]: https://img.shields.io/badge/license-MIT_OR_Apache--2-blue.svg
[license-url]: LICENSE-APACHE
[actions-badge]: https://github.com/collabora/gitlab-runner-rs/workflows/CI/badge.svg
[actions-url]:https://github.com/collabora/gitlab-runner-rs/actions?query=workflow%3ACI


[GitLab](https://gitlab.com) provides a REST style API for CI runners. These
crates abstract this API away so custom runners can easily be build without
any knowledge of the gitlab APIs

This workspace consists of two crates:
* [gitlab-runner](gitlab-runner/README.md) - Main runner crate
* gitlab-runner-mock - Mock crate for the gitlab runner APIs
