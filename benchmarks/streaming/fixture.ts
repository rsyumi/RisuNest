import "../../src/styles.css";
import { mount, settled as domSettled, unmount } from "svelte";
import Fixture from "./Fixture.svelte";
import { DBState, selectedCharID } from "../../src/ts/stores.svelte";
import {
  normalizeDatabaseDefaults,
  type Database,
} from "../../src/ts/storage/database.svelte";
import { changeLanguage } from "../../src/lang";
import { updateColorScheme } from "../../src/ts/gui/colorscheme";
import { processScriptFull } from "../../src/ts/process/scripts";
import { runStreamingSuite } from "./suite";
import { runLeaseReleaseCheckpointSuite } from "./lease-release";
import { runHypaSummaryProbe } from "./hypaSummaryProbe";
import {
  runPersistenceSpike,
  runPersistenceSuite,
  checkPersistenceReload,
} from "./persistence";

import { viewportProbe } from "./viewport";
import { measureParserContext } from './parserContext.svelte';

export function startStreamingSmoke() {
  const character = {
    type: "character",
    chaId: "streaming-synthetic",
    name: "Synthetic",
    chatPage: 0,
    image: "",
    firstMessage: "",
    desc: "",
    notes: "",
    chats: [
      {
        id: "streaming-chat",
        name: "Synthetic",
        message: [],
        note: "",
        localLore: [],
      },
    ],
    chatFolders: [],
    emotionImages: [],
    additionalAssets: [],
    bias: [],
    viewScreen: "none",
    globalLore: [],
    sdData: [],
    utilityBot: false,
    triggerscript: [],
    exampleMessage: "",
    creatorNotes: "",
    systemPrompt: "",
    postHistoryInstructions: "",
    replaceGlobalNote: "",
    alternateGreetings: [],
    tags: [],
    creator: "",
    characterVersion: "",
    personality: "",
    scenario: "",
    firstMsgIndex: -1,
    additionalText: "",
    customscript: [
      {
        comment: "Synthetic final thought removal",
        in: "<Thoughts>[\\s\\S]*?</Thoughts>",
        out: "",
        type: "editdisplay",
        flag: "g",
      },
    ],
  };
  const removalScripts = character.customscript;
  DBState.db = normalizeDatabaseDefaults({
    characters: [character],
    username: "Synthetic User",
    modules: [],
    plugins: [],
    botPresets: [],
    regex: [],
    globalChatVariables: {},
    autoTranslate: false,
    animation: false,
  } as unknown as Database);
  selectedCharID.set(0);
  changeLanguage("en");
  updateColorScheme();
  const fixture = mount(Fixture, { target: document.getElementById("app")! });
  const api = {
    marker: "synthetic-v1",
    configure: async (mode: "recent" | "collapsed" | "off", defer: boolean) => {
      fixture.configure(mode, defer);
      await domSettled();
    },
    publish: async (source: string, active = true) => {
      fixture.publish(source, active);
      await domSettled();
    },
    sourceMatches: fixture.sourceMatches,
    debugState: () => ({
      dbScriptCount: DBState.db.characters[0].customscript.length,
      ...fixture.inspectScripts(),
    }),
    debugFinal: async (source: string) => {
      const result = await processScriptFull(
        DBState.db.characters[0],
        source,
        "editdisplay",
        -1,
        { chatRole: "char" },
        { cache: "bypass" },
      );
      return {
        directHasLatestThought: result.data.includes("LATEST-SYNTHETIC"),
        directHasRawAnswerMarkdown: result.data.includes("**Answer**"),
        directLength: result.data.length,
      };
    },
    setRemoval: async (enabled: boolean) => {
      const scripts = enabled ? removalScripts : [];
      DBState.db.characters[0].customscript = scripts;
      fixture.setScripts(scripts);
      await domSettled();
    },
  };
  let regexFixtureStarted = false;
  const startRegexFixture = async () => {
    if (regexFixtureStarted) return;
    regexFixtureStarted = true;
    await unmount(fixture);
  };
  Object.assign(window, {
    __streamingSmoke: {
      ...api,
      viewport: viewportProbe,
      measureParserContext,
      regexTimeLimit: (counts?: number[], sizes?: number[], repeats?: number, rows?: number, workload?: import('./regexTimeLimitProbe').RegexProbeWorkload) => import('./regexTimeLimitProbe')
        .then(({ runRegexTimeLimitProbe }) => runRegexTimeLimitProbe(startRegexFixture, counts, sizes, repeats, rows, workload)),
      regexTimeout: () => import('./regexTimeLimitProbe').then(({ verifyRegexTimeoutResponsiveness }) => verifyRegexTimeoutResponsiveness()),
      parserScreen: (assets: number, baseline = false) => import('./parserScreenProbe')
        .then(({ runParserScreenProbe }) => runParserScreenProbe(assets, baseline, () => unmount(fixture))),
      checkPersistenceReload,
      run: (profile = "smoke") =>
        profile === 'regex-time-limit' || profile === 'regex-cbs'
          ? import('./regexTimeLimitProbe').then(({ runRegexTimeLimitProbe }) => runRegexTimeLimitProbe(startRegexFixture, undefined, undefined, undefined, undefined, profile === 'regex-cbs' ? 'cbs' : 'mixed'))
          : profile === "asset-bytes" || profile === "asset-bytes-legacy"
            ? import('../native-assets/suite').then(async ({ runNativeAssetMeasurements, verifyConcurrentNativeByteWriters }) => {
              const { SqlitePersistentDataStore } = await import('../../src/ts/storage/sqlitePersistentDataStore')
              const store = new SqlitePersistentDataStore()
              await store.open()
              const descriptor = Object.getOwnPropertyDescriptor(window, 'RisuNestCommit')
              try {
                if (profile === 'asset-bytes-legacy') Object.defineProperty(window, 'RisuNestCommit', { configurable: true, value: undefined })
                const measurements = await runNativeAssetMeasurements(store)
                const concurrency = await verifyConcurrentNativeByteWriters(store)
                return { passed: true, cases: [{ id: 'native-asset-bytes', measurements }, { id: 'concurrent-byte-writers', ...concurrency }] }
              } finally {
                if (descriptor) Object.defineProperty(window, 'RisuNestCommit', descriptor)
              }
            })
          : profile === "viewport"
          ? unmount(fixture).then(() => viewportProbe.run())
          : profile === "hypa-summary"
            ? runHypaSummaryProbe(() => unmount(fixture))
            : profile === "persistence-spike"
              ? runPersistenceSpike()
              : profile === "persistence"
                ? runPersistenceSuite(api)
                : profile === "lease-release"
                  ? runLeaseReleaseCheckpointSuite(api)
                  : runStreamingSuite(api, profile),
    },
  });
}
