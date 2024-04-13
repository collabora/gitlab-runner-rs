# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.9](https://github/collabora/gitlab-runner-rs/compare/gitlab-runner-v0.0.8...gitlab-runner-v0.0.9) - 2024-04-13

### Added
- Move runner creation to builder pattern
- Seperate logging layer creation from runner creation
- Add automatic generation of system_id for the runner
- Allow setting runner metadata

### Other
- Add test for metadata
- Add metadata
- Allow setting runner metadata
- Update reqwest requirement from 0.11.3 to 0.12.0
- Improve logging on demo runner
- Add a specific span target adding the gitlab job id
- Add a per-layer filter on top of the GitlabLayer
- re-add root README.md
- Deduplicate README and crate documentation
- Update to wiremock 0.0.6
- update README.md for the new registration workflow
- Add automatic generation of system_id for the runner
- Bump dependencis as need to build with minimal-versions
