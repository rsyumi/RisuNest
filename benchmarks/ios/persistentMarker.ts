import { SqlitePersistentDataStore } from "../../src/ts/storage/sqlitePersistentDataStore";

export class PersistentBenchmarkMarker {
  private readonly store = new SqlitePersistentDataStore();
  private revision: number | undefined;

  async open(): Promise<void> {
    await this.store.open();
    this.revision = this.store.lastOpenResult?.revision;
    if (this.revision === undefined)
      throw new Error("Persistent benchmark store did not report a revision");
  }

  async read(): Promise<string> {
    const root = await this.store.readRoot();
    this.revision = root.revision;
    return root.value.username;
  }

  async write(value: string): Promise<void> {
    if (this.revision === undefined)
      throw new Error("Persistent benchmark store is not open");
    const result = await this.store.commit({
      expectedRevision: this.revision,
      rootMutations: [{ type: "set", key: "username", value }],
    });
    this.revision = result.revision;
  }
}
