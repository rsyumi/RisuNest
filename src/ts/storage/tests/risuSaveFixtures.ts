import type { Database } from '../database.svelte'

function fromBase64(value: string): Uint8Array {
    return Uint8Array.from(Buffer.from(value, 'base64'))
}

export const risuSaveFixtureDatabase = {
    username: 'Snapshot User',
    formatversion: 4,
    apiType: 'fixture-provider',
    botPresets: [{ name: 'Fixture preset' }],
    botPresetsId: 0,
    modules: [{ name: 'Fixture module' }],
    loadouts: [{ name: 'Fixture loadout' }],
    plugins: [{ name: 'Fixture plugin' }],
    pluginCustomStorage: { fixture: { value: 'stored' } },
    characters: [
        {
            type: 'character',
            chaId: 'char-fixture',
            name: 'Fixture Character',
            image: 'fixture.png',
            lastInteraction: 100,
            chats: [
                {
                    id: 'chat-fixture',
                    name: 'Fixture Chat',
                    lastDate: 100,
                    message: [
                        { role: 'user', data: 'hello', chatId: 'message-1', time: 100 },
                        { role: 'char', data: 'world', chatId: 'message-2', time: 101 },
                    ],
                },
            ],
        },
    ],
} as unknown as Database

export const rawRisuSaveFixture = fromBase64(
    'AFJJU1VTQVZFAAfeAAqodXNlcm5hbWWtU25hcHNob3QgVXNlcq1mb3JtYXR2ZXJzaW9uBKdhcGlUeXBlsGZpeHR1cmUtcHJvdmlkZXKqYm90UHJlc2V0c5HeAAGkbmFtZa5GaXh0dXJlIHByZXNldKxib3RQcmVzZXRzSWQAp21vZHVsZXOR3gABpG5hbWWuRml4dHVyZSBtb2R1bGWobG9hZG91dHOR3gABpG5hbWWvRml4dHVyZSBsb2Fkb3V0p3BsdWdpbnOR3gABpG5hbWWuRml4dHVyZSBwbHVnaW6zcGx1Z2luQ3VzdG9tU3RvcmFnZd4AAadmaXh0dXJl3gABpXZhbHVlpnN0b3JlZKpjaGFyYWN0ZXJzkd4ABqR0eXBlqWNoYXJhY3RlcqVjaGFJZKxjaGFyLWZpeHR1cmWkbmFtZbFGaXh0dXJlIENoYXJhY3RlcqVpbWFnZatmaXh0dXJlLnBuZ69sYXN0SW50ZXJhY3Rpb25kpWNoYXRzkd4ABKJpZKxjaGF0LWZpeHR1cmWkbmFtZaxGaXh0dXJlIENoYXSobGFzdERhdGVkp21lc3NhZ2WS3gAEpHJvbGWkdXNlcqRkYXRhpWhlbGxvpmNoYXRJZKltZXNzYWdlLTGkdGltZWTeAASkcm9sZaRjaGFypGRhdGGld29ybGSmY2hhdElkqW1lc3NhZ2UtMqR0aW1lZQ==',
)

export const compressedRisuSaveFixture = fromBase64(
    'AFJJU1VTQVZFAAgfiwgAtKSIagADbZFfTgIxEIcxIT54Ci6AiV4BY7JvJugBRjpAk3anaaeox9AjmIUFBNF4H87idHdBJDz1z+/rN51227qYx4A+B4vrfg4ujIk7D7KzHpK3wBP0QVPeLsHp+xeHX0P9zNFj13maaIV++Uh85zEgh9dt66xIps/bGuq4Klj9IZlqlZZUNHiCroO5IVAUD3SbHdAkpTNxpPNT9argpx56MTDZPpOHEQpaNleX6XQCJuJMco9qORiDhwFLp2I8L1jaXOz3pjLL1Cqtu42gqvm9q9nbk9pKoY8GunT5aGMgcJZLJoC8okqyqrH2u66c/M+5OnDyPB2+AUZVWgxB1G9yrvBksEhfVihgmI7RGJolU6YWDde9KlhbVHs8Xb7Gn8gbdYxfVzj+Ai6VUGEMAgAA',
)

export const streamRisuSaveFixture = fromBase64(
    'AFJJU1VTQVZFAAkfiwgAAAAAAAAKbZFRTgIxEIaXhPjgKbgAJnoFjMm+maAHGOmw26TbaaZTlGPoEczCAoJovM+exXR3QTQ+Ne3/9funaZ2cr4JHtlDgbmzB+ZxkcO+Rd1PiAmSG7DXZfgVO380dfkz1kwTGoWOaaYW8eSC5ZfQo/rlOemU0vd+00MA1wfYHSVVSFaSCwX/oNlgZAkXhRLc/AF1SORMybf/ra4KvdhkFL1SMhRgyrJNe1Y1eJ73FDEzApRdiVJtJDgwTQY7Gs1LmDtfHs8Ukh1Rt437YCZrOz0Pn6EjqAjJ866ALZ7O9AS+pFYyAJquirHlY/1U3Tvnl3J44ZRUvX4Ogqgr0HjJ8qZN+yWSwjF9WKhBY5GgMLaMpVeuOG16WogtURzwO3+KPxEb9xa8aHL8BLpVQYQwCAAA=',
)

export const blockRisuSaveFixture = fromBase64(
    'UklTVVNBVkUAAQEEcm9vdKAAAAAfiwgAAAAAAAAKRY1BCsIwEEWvIn+dggtXuYE7oboSKbGZpoE2EyaTYhHvLi2Cu/f4D/4btZAkNxMs2uRyGVkPt0ICg4FldrqQlMgJ9mTgcryueWuH+NIq1GThJfo9f7JehAppOXvYo0HX+SjUK8sKe0feRxjM7OtEBQYTO89VN8xTDTH9qVUWFwgG/eik+f1tymmIAY/PF3ggMki8AAAABAEGcHJlc2V0LwAAAB+LCAAAAAAAAAqLrlbKS8xNVbJScsusKCktSlUoKEotTi1Rqo0FAEeYHScbAAAABQEHbW9kdWxlcy8AAAAfiwgAAAAAAAAKi65WykvMTVWyUnLLrCgpLUpVyM1PKc1JVaqNBQC5o7cIGwAAAAoBCGxvYWRvdXRzMAAAAB+LCAAAAAAAAAqLrlbKS8xNVbJScsusKCktSlXIyU9MyS8tUaqNBQASdJDpHAAAAAkBB3BsdWdpbnMvAAAAH4sIAAAAAAAACouuVspLzE1VslJyy6woKS1KVSjIKU3PzFOqjQUA5z3hSxsAAAALAQ1wbHVnaW5TdG9yYWdlMgAAAB+LCAAAAAAAAAqrVkrLrCgpLUpVsqpWKkvMKU1VslIqLskvSk1Rqq0FAP+COgceAAAAAgEMY2hhci1maXh0dXJltwAAAB+LCAAAAAAAAAp9j80KwjAQhF9F5pxK6zFXRegzFA+hjW0gPyXZolLy7pI0aA/ibdmdb2ZnBb1mCY5+El70JD1Ymtuh7Kq7etLiJRisMEl53RaH845QRozpVsTH2Y5g0CJQa0kmmXIWvKnr7E4BvFuhSgj9DaHidBEki4WRIeTAboV3OgFLyI8MggQ4Jqm125pQrlKIqgEDKbMZRfbBU9Uv/nBeD7/w0w5v4i3e4huX6/m+QQEAAAABBmNvbmZpZyEAAAAfiwgAAAAAAAAKq1YqSy0qzszPU7IyrAUAM5owcQ0AAAA=',
)

export const risuSaveFixtures = [
    ['raw', rawRisuSaveFixture],
    ['compressed', compressedRisuSaveFixture],
    ['stream', streamRisuSaveFixture],
    ['block', blockRisuSaveFixture],
] as const
