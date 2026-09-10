import "../../src/styles.css";
import { mount, tick } from "svelte";
import Fixture from "./Fixture.svelte";
import { DBState, selectedCharID } from "../../src/ts/stores.svelte";
import {
  normalizeDatabaseDefaults,
  type Database,
} from "../../src/ts/storage/database.svelte";
import { changeLanguage } from "../../src/lang";
import { updateColorScheme } from "../../src/ts/gui/colorscheme";
import { runStreamingSuite } from "./suite";
import {
  runPersistenceSpike,
  runPersistenceSuite,
  checkPersistenceReload,
} from "./persistence";

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
      await tick();
    },
    publish: async (source: string, active = true) => {
      fixture.publish(source, active);
      await tick();
    },
    sourceMatches: fixture.sourceMatches,
    setRemoval: (enabled: boolean) => {
      DBState.db.characters[0].customscript = enabled ? removalScripts : [];
    },
  };
  Object.assign(window, {
    __streamingSmoke: {
      ...api,
      checkPersistenceReload,
      run: (profile = "smoke") =>
        profile === "persistence-spike"
          ? runPersistenceSpike()
          : profile === "persistence"
            ? runPersistenceSuite(api)
            : runStreamingSuite(api, profile),
    },
  });
}
