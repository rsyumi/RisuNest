/**
 * A class to manage a buffer that can be appended to and deappended from.
 */
export class AppendableBuffer {
    deapended: number = 0
    #buffer: Uint8Array
    #byteLength: number = 0

    /**
     * Creates an instance of AppendableBuffer.
     */
    constructor() {
        this.#buffer = new Uint8Array(128)
    }

    get buffer(): Uint8Array {
        return this.#buffer.slice(0, this.#byteLength)
    }

    /**
     * Appends data to the buffer.
     * @param {Uint8Array} data - The data to append.
     */
    append(data: Uint8Array) {
        // New way (faster)
        const requiredLength = this.#byteLength + data.length
        if (this.#buffer.byteLength < requiredLength) {
            let newLength = this.#buffer.byteLength * 2
            while (newLength < requiredLength) {
                newLength *= 2
            }
            const newBuffer = new Uint8Array(newLength)
            newBuffer.set(this.#buffer)
            this.#buffer = newBuffer
        }
        this.#buffer.set(data, this.#byteLength)
        this.#byteLength += data.length
    }

    /**
     * Deappends a specified length from the buffer.
     * @param {number} length - The length to deappend.
     */
    deappend(length: number) {
        this.#buffer = this.#buffer.slice(length)
        this.deapended += length
        this.#byteLength -= length
    }

    /**
     * Slices the buffer from start to end.
     * @param {number} start - The start index.
     * @param {number} end - The end index.
     * @returns {Uint8Array} - The sliced buffer.
     */
    slice(start: number, end: number) {
        return this.buffer.slice(start - this.deapended, end - this.deapended)
    }

    /**
     * Gets the total length of the buffer including deappended length.
     * @returns {number} - The total length.
     */
    length() {
        return this.#byteLength + this.deapended
    }

    /**
     * Clears the buffer.
     */
    clear() {
        this.#buffer = new Uint8Array(128)
        this.#byteLength = 0
        this.deapended = 0
    }
}
