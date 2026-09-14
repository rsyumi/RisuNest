import { readFileSync } from "node:fs";

const EXPECTED = {
  x86_64: { pe: 0x8664, elf: 62, macho: 0x01000007 },
  aarch64: { pe: 0xaa64, elf: 183, macho: 0x0100000c },
};

function peMachine(bytes) {
  if (bytes.length < 64 || bytes.toString("ascii", 0, 2) !== "MZ") return null;
  const offset = bytes.readUInt32LE(0x3c);
  if (offset + 6 > bytes.length || bytes.toString("ascii", offset, offset + 4) !== "PE\0\0")
    return null;
  return bytes.readUInt16LE(offset + 4);
}

function elfMachine(bytes) {
  if (bytes.length < 20 || !bytes.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46])))
    return null;
  if (bytes[4] !== 2) throw new Error("Expected a 64-bit ELF binary.");
  if (bytes[5] === 1) return bytes.readUInt16LE(18);
  if (bytes[5] === 2) return bytes.readUInt16BE(18);
  throw new Error("Unknown ELF byte order.");
}

function machoCpus(bytes) {
  if (bytes.length < 8) return null;
  const littleMagic = bytes.readUInt32LE(0);
  if (littleMagic === 0xfeedfacf) return [bytes.readUInt32LE(4)];
  const bigMagic = bytes.readUInt32BE(0);
  if (bigMagic === 0xfeedfacf) return [bytes.readUInt32BE(4)];
  if (bigMagic === 0xcafebabe || bigMagic === 0xcafebabf) {
    const count = bytes.readUInt32BE(4);
    const stride = bigMagic === 0xcafebabf ? 32 : 20;
    if (count < 1 || 8 + count * stride > bytes.length) throw new Error("Invalid universal Mach-O header.");
    return Array.from({ length: count }, (_, index) => bytes.readUInt32BE(8 + index * stride));
  }
  return null;
}

export function binaryArchitecture(path) {
  const bytes = readFileSync(path);
  const pe = peMachine(bytes);
  if (pe !== null) {
    for (const [arch, values] of Object.entries(EXPECTED)) if (values.pe === pe) return { format: "pe", arches: [arch] };
    return { format: "pe", arches: [`machine-0x${pe.toString(16)}`] };
  }
  const elf = elfMachine(bytes);
  if (elf !== null) {
    for (const [arch, values] of Object.entries(EXPECTED)) if (values.elf === elf) return { format: "elf", arches: [arch] };
    return { format: "elf", arches: [`machine-${elf}`] };
  }
  const cpus = machoCpus(bytes);
  if (cpus !== null) {
    const arches = cpus.map((cpu) => Object.entries(EXPECTED).find(([, values]) => values.macho === cpu)?.[0] ?? `cpu-0x${cpu.toString(16)}`);
    return { format: "macho", arches };
  }
  throw new Error(`${path} is not a recognized PE, ELF, or Mach-O executable.`);
}

export function assertBinaryArchitecture(path, expectedArch, expectedFormat) {
  const result = binaryArchitecture(path);
  if (expectedFormat && result.format !== expectedFormat)
    throw new Error(`${path} is ${result.format}, expected ${expectedFormat}.`);
  if (result.arches.length !== 1 || result.arches[0] !== expectedArch)
    throw new Error(`${path} targets ${result.arches.join(",")}, expected ${expectedArch}.`);
  return result;
}
