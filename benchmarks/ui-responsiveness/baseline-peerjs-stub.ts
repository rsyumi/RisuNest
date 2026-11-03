// Baseline multiuser rooms load peerjs on demand, and the working tree no
// longer installs it. The benchmark never opens a room.
export class Peer {
    constructor() {
        throw new Error('peerjs is not installed')
    }
}

export default Peer
