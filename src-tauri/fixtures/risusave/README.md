# Native RisuSave export goldens

These byte fixtures were captured from the persistent-store exporter at commit `d9c44a9f`, before the desktop native-file-job route was added. They deliberately do not call the current encoder when forming the expected result.

| Fixture | omitAccount | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| `pre-native-job-export-omit-account-false.b64` | false | 813 | `2b84dcc0abeafc7354354e0d1fb3360443011bc73bb364ffa24180f29f0e0baa` |
| `pre-native-job-export-omit-account-true.b64` | true | 796 | `29dfc33b0f964cd3d54791fb92a12565f393e7710f713622cd52b667e66d3921` |

The source database is the historical `persistent_store::export` test fixture: `Native Export`, two presets, the trash-first and live-second characters, modules, loadouts, plugins, plugin storage, config, and an account token.
