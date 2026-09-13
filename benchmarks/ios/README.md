# iOS native verification

This harness uses synthetic fixtures, a separate app identifier, and the product
builder and command router. Run it in agent mode. Never point it at installed app
data or use live character libraries.

The build-lab `ios-check.yml` workflow builds the product and harness for the
simulator, then runs storage, restart, streaming, file, lifecycle, product UI and
XCTest checks. The controller remains in build-lab and consumes immutable audited
source snapshots.

## Firebase Test Lab preparation

The manual build-lab `ios-testlab-package.yml` workflow runs `buildTestLab.py` on
macOS with Xcode. It produces:

- `RisuNest-ios-testlab.zip`: `Debug-iphoneos`, the isolated app, signed UI test
  runner, and `.xctestrun` configuration. Tests cover synthetic file publication
  and import, notification permission, external URL opening, product chat
  persistence and keyboard input, and a measured app transition.
- `RisuNest-ios-device-agent.ipa`: the product agent build, separate from the
  verification app. This is not a distribution-signed or TestFlight build.
- `testlab-package.json`: source provenance, Xcode, target, size and SHA-256.

The script verifies arm64/iphoneos and ad-hoc signatures locally. Firebase
re-signs submitted apps. The first submitted package (source `d94ec8296`, Xcode
26.2) was accepted and passed all four UI tests on iPad 10/iOS 16.6 and iPhone SE
3/iOS 26.3. Each later package still requires its own device validation. No
Firebase SDK or credentials are added to the product.

The device-core test additionally measures alternating JSON and worker/raw
storage commits, exact Unicode and revision contracts, native regex against its
JavaScript oracle, and tokenizer results. It attaches measurements instead of
asserting a speedup: simulator trials showed inconsistent storage timing gains.

After Firebase is configured, inspect its current device and Xcode catalog:

```sh
gcloud firebase test ios models list --project=YOUR_PROJECT
gcloud firebase test ios versions list --project=YOUR_PROJECT
```

Select a supported iPhone and iPad, and an Xcode version compatible with their OS
versions. Rebuild using that workflow input when needed. Then upload the XCTest
ZIP in the Firebase console or run the equivalent command with verified IDs:

```sh
gcloud firebase test ios run --project=YOUR_PROJECT --type=xctest \
  --test=RisuNest-ios-testlab.zip --timeout=15m \
  --device=model=VERIFIED_MODEL,version=VERIFIED_OS,locale=en_US,orientation=portrait \
  --xcode-version=VERIFIED_XCODE
```

UI selectors use English system labels. The app-transition test attaches its
measured ticks, execution gap and native state; it does not assert unlimited
background execution. Test Lab runs are bounded and do not replace long locked
device, battery/thermal, personal iCloud provider or camera-content validation.
No service upload or paid execution happens as part of the package workflow.

References: [XCTest packaging](https://firebase.google.com/docs/test-lab/ios/run-xctest),
[device catalog](https://firebase.google.com/docs/test-lab/ios/available-testing-devices),
[CLI](https://docs.cloud.google.com/sdk/gcloud/reference/firebase/test/ios/run).
