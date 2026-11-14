export function luaCodeWrapper(code:string){
    return `
json = require 'json'

-- The script shares this chunk's limit of 200 locals, so the wrapper keeps upstream's four.
do
    local original = json
    local decode = json.decode
    local kind, rawnext = type, next
    local getmeta = getmetatable
    local stringMeta = getmeta('')
    local stringIndex = stringMeta.__index
    local originals = {string.byte, string.find, string.sub, string.match, string.format, string.char, math.floor,
        tonumber, tostring, error, type, pairs, table.insert, table.concat}
    local entries, size = {}, 0
    local function copy(value)
        if kind(value) ~= 'table' then return value end
        local result = {}
        for key, item in rawnext, value do result[key] = copy(item) end
        return result
    end
    local function decodeJSON(value)
        local current = {string.byte, string.find, string.sub, string.match, string.format, string.char, math.floor,
            tonumber, tostring, error, type, pairs, table.insert, table.concat}
        if json ~= original or json.decode ~= decode then return json.decode(value) end
        if getmeta('') ~= stringMeta or stringMeta.__index ~= stringIndex then return json.decode(value) end
        for i = 1, #originals do
            if current[i] ~= originals[i] then return json.decode(value) end
        end
        if kind(value) ~= 'string' or #value > 512 * 1024 then return decode(value) end
        for i = 1, #entries do
            if entries[i].source == value then return copy(entries[i].value) end
        end
        local result = decode(value)
        if #entries >= 4 or size + #value > 512 * 1024 then entries, size = {}, 0 end
        entries[#entries + 1] = {source = value, value = result}
        size = size + #value
        return copy(result)
    end

    function getChat(id, index)
        return decodeJSON(getChatMain(id, index))
    end

    function getFullChat(id)
        return decodeJSON(getFullChatMain(id))
    end

    function getRecentChats(id, count)
        return decodeJSON(getRecentChatsMain(id, count))
    end

    function getLoreBooks(id, search)
        return decodeJSON(getLoreBooksMain(id, search))
    end

    function getState(id, name)
        local escapedName = "__"..name
        return decodeJSON(getChatVar(id, escapedName))
    end
end

function setFullChat(id, value)
    setFullChatMain(id, json.encode(value))
end

function log(value)
    logMain(json.encode(value))
end


function loadLoreBooks(id)
    return json.decode(loadLoreBooksMain(id):await())
end

function LLM(id, prompt, useMultimodal, options)
    useMultimodal = useMultimodal or false
    options = options or {}
    return json.decode(LLMMain(id, json.encode(prompt), useMultimodal, json.encode(options)):await())
end

function axLLM(id, prompt, useMultimodal, options)
    useMultimodal = useMultimodal or false
    options = options or {}
    return json.decode(axLLMMain(id, json.encode(prompt), useMultimodal, json.encode(options)):await())
end

function getCharacterImage(id)
    return getCharacterImageMain(id):await()
end

function getPersonaImage(id)
    return getPersonaImageMain(id):await()
end

local editRequestFuncs = {}
local editDisplayFuncs = {}
local editInputFuncs = {}
local editOutputFuncs = {}

function listenEdit(type, func)
    if type == 'editRequest' then
        editRequestFuncs[#editRequestFuncs + 1] = func
        return
    end

    if type == 'editDisplay' then
        editDisplayFuncs[#editDisplayFuncs + 1] = func
        return
    end

    if type == 'editInput' then
        editInputFuncs[#editInputFuncs + 1] = func
        return
    end

    if type == 'editOutput' then
        editOutputFuncs[#editOutputFuncs + 1] = func
        return
    end

    throw('Invalid type')
end

function setState(id, name, value)
    local escapedName = "__"..name
    setChatVar(id, escapedName, json.encode(value))
end

function setStateChanged(id, name, value)
    local escapedName = "__"..name
    return setChatVarChanged(id, escapedName, json.encode(value))
end

function async(callback)
    return function(...)
        local co = coroutine.create(callback)
        local safe, result = coroutine.resume(co, ...)

        return Promise.create(function(resolve, reject)
            local checkresult
            local step = function()
                if coroutine.status(co) == "dead" then
                    local send = safe and resolve or reject
                    return send(result)
                end

                safe, result = coroutine.resume(co)
                checkresult()
            end

            checkresult = function()
                if safe and result == Promise.resolve(result) then
                    result:finally(step):catch(reject)
                else
                    step()
                end
            end

            checkresult()
        end)
    end
end

callListenMain = async(function(eventType, id, value, meta)
    local realValue = value
    local realMeta = json.decode(meta)

    if eventType == 'editDisplay' then
        local realValue = value
        for _, func in ipairs(editDisplayFuncs) do
            local output = func(id, realValue, realMeta)
            local outputType = type(output)
            if outputType == 'string' then
                realValue = output
            else
                print('Error: Lua editDisplay must return a string, received ' .. outputType)
            end
        end
        return realValue
    end

    realValue = json.decode(value)

    if eventType == 'editRequest' then
        for _, func in ipairs(editRequestFuncs) do
            realValue = func(id, realValue, realMeta)
        end
    end

    if eventType == 'editInput' then
        for _, func in ipairs(editInputFuncs) do
            realValue = func(id, realValue, realMeta)
        end
    end

    if eventType == 'editOutput' then
        for _, func in ipairs(editOutputFuncs) do
            realValue = func(id, realValue, realMeta)
        end
    end

    return json.encode(realValue)
end)

${code}
`
}
