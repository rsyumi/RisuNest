import type { OutputPluginDispatch } from "../request/request";
import { MCPClientLike } from "./internalmcp";
import type { MCPTool, RPCToolCallContent } from "./mcplib";

export const registeredCustomPluginMCPs: Map<string, CustomPluginMCPClient> = new Map();

export class CustomPluginMCPClient extends MCPClientLike {

    #getToolList: (() => Promise<MCPTool[]>) | null;
    #callTool: ((toolName: string, content: any) => Promise<RPCToolCallContent[]>) | null;
    constructor(arg: {
        identifier: string;
        name: string;
        version: string;
        description: string;
        getToolList: () => Promise<MCPTool[]>;
        callTool: (toolName: string, content: any) => Promise<RPCToolCallContent[]>;
    }) {
        super(arg.identifier);
        this.serverInfo.serverInfo.name = arg.name;
        this.serverInfo.serverInfo.version = arg.version;
        this.serverInfo.instructions = arg.description;
        this.#getToolList = arg.getToolList;
        this.#callTool = arg.callTool;
    }

    async getToolList(dispatch?: OutputPluginDispatch) {
        if (dispatch && this.#getToolList) {
            await dispatch.beforePlugin();
            dispatch.signal.throwIfAborted();
        }
        if (!this.#getToolList) return [];
        return await this.#getToolList();
    }

    async callTool(toolName: string, args: any, dispatch?: OutputPluginDispatch) {
        if (dispatch && this.#callTool) {
            await dispatch.beforePlugin();
            dispatch.signal.throwIfAborted();
        }
        if (!this.#callTool) throw new Error('Plugin MCP is no longer registered');
        return await this.#callTool(toolName, args);
    }

    revoke() {
        this.#getToolList = null;
        this.#callTool = null;
    }

}

export async function registerMCPModule(arg: {
    identifier: string;
    name: string;
    version: string;
    description: string;
}, 
    getToolList: () => Promise<MCPTool[]>,
    callTool: (toolName: string, content: any) => Promise<RPCToolCallContent[]>,
) {
    if (!arg.identifier.startsWith('plugin:')) {
        throw new Error(`MCP module identifier must start with 'plugin:'.`);
    }
    const client = new CustomPluginMCPClient({
        ...arg,
        getToolList,
        callTool,
    });
    registeredCustomPluginMCPs.get(arg.identifier)?.revoke();
    registeredCustomPluginMCPs.set(arg.identifier, client);
    return client;
}

export async function unregisterMCPModule(identifier: string) {
    registeredCustomPluginMCPs.get(identifier)?.revoke();
    registeredCustomPluginMCPs.delete(identifier);
}
