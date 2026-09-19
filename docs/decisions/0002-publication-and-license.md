# 0002: Public repository and MIT license

Date: 2026-09-19
Status: Accepted
Supersedes: visibility and licensing portions of [0001](0001-project-baseline.md)

## Decision

Publish Latchrun as a public GitHub repository and license the code and documentation under MIT. The owner explicitly requested public sharing after reviewing the initial private default and delegated selection of an appropriate license.

MIT supports the stated goal of broad reuse, including modification and commercial redistribution, with preservation of the copyright and license notice. It includes warranty and liability disclaimers. See the [standard license description](https://choosealicense.com/licenses/mit/) and [LICENSE](../../LICENSE).

## Consequences

Review tracked content, Git history, and existing hosted CI records before changing visibility; the [publication review](../publication-review.md) records the initial inventory and findings. Retain existing public author attribution. Avoid putting private infrastructure details or credentials into future code, examples, logs, or issues.

Public source availability does not imply release readiness or a security guarantee. Cargo package publication remains disabled until a separate release decision. No other repository's visibility changes as part of this decision.
