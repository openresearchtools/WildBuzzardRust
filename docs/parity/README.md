# Parity evidence

Parity means observable standards and product behavior, not matching Firefox's internal implementation. Every tracked feature must eventually record:

- the Firefox ESR reference source and tests;
- the applicable WPT, Test262, WebAssembly, reftest, WebDriver, or accessibility tests;
- Wild Buzzard implementation and contract paths;
- commands and platforms on which the tests passed;
- intentional differences and their security or product rationale;
- remaining failures, skips, crashes, performance gaps, and unsupported behavior.

Passing compilation is not parity. Unsupported behavior must be visible in this registry and fail safely.
