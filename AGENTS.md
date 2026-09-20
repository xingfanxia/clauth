# clauth — agent guide

## Project scale and verification

**Profile: personal credential manager.** Clauth switches real accounts through Keychain and a daemon. TUI changes can be checked locally; credential writes, OAuth, socket commands and switch/recovery behavior need focused tests with synthetic state. Preserve existing credentials and release CI; never use real account switching as an unannounced test.

- The requested behavior/questions define completion. Reviews are read-only unless fixes are requested; report unrelated findings briefly without adding tasks or test backfill.
- Use the smallest existing check that proves the change. Add tests for a concrete regression or consequential boundary; do not impose blanket TDD, new coverage targets, full suites, plans or reviewers. Preserve configured CI and actual release gates; reuse still-valid results.
- Keep the existing structure. Internal contract errors should be clear; add retries, fallbacks or compatibility layers only for an observed external failure or supported contract. Keep secrets private and inspect security only at boundaries changed by this task.

