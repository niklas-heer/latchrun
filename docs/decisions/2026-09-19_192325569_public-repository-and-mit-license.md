+++
schema_version = 1
id = "01M2XHZ6G1ZDCCHXNSQ8W2Z95C"
title = "Public repository and MIT license"
date = "2026-09-19"
status = "accepted"
tags = ["licensing", "git"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M2XHZ6FS4TWZ94F2J10H3M2B"]
+++
Supersedes: visibility and licensing portions of [0001](2026-09-19_192325561_rust-project-and-development-baseline.md)

## Decision

Publish Latchrun as a public GitHub repository and license the code and documentation under MIT. The owner explicitly requested public sharing after reviewing the initial private default and delegated selection of an appropriate license.

MIT supports the stated goal of broad reuse, including modification and commercial redistribution, with preservation of the copyright and license notice. It includes warranty and liability disclaimers. See the [standard license description](https://choosealicense.com/licenses/mit/) and [LICENSE](../../LICENSE).

## Consequences

Review tracked content, Git history, and existing hosted CI records before changing visibility; the [publication review](../publication-review.md) records the initial inventory and findings. Retain existing public author attribution. Avoid putting private infrastructure details or credentials into future code, examples, logs, or issues.

Public source availability does not imply release readiness or a security guarantee. Cargo package publication remains disabled until a separate release decision. No other repository's visibility changes as part of this decision.
