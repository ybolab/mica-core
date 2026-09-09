/** Boot a complete current UEFI image with an explicitly enrolled test key. */
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { seedArguments, seedDataImage } from "../../../../../build/src/seed-data.ts";
import { parseFileLayout } from "../../../../../build/src/file-layout.ts";

interface QemuArch {
  readonly binary: string;
  readonly machine: string;
  readonly packages: string;
  readonly firmwareCode: string;
  readonly firmwareVars: string;
}

export const QEMU_ARCHES: Readonly<Record<string, QemuArch>> = {
  amd64: {
    binary: "qemu-system-x86_64", machine: "q35", packages: "qemu-system-x86 ovmf",
    firmwareCode: "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd", firmwareVars: "/usr/share/OVMF/OVMF_VARS_4M.fd",
  },
  arm64: {
    binary: "qemu-system-aarch64", machine: "virt", packages: "qemu-system-arm qemu-efi-aarch64 ipxe-qemu",
    firmwareCode: "/usr/share/AAVMF/AAVMF_CODE.secboot.fd", firmwareVars: "/usr/share/AAVMF/AAVMF_VARS.fd",
  },
};

export function qemuArchFor(arch: string | undefined, board: string): QemuArch {
  const spec = arch === undefined ? undefined : QEMU_ARCHES[arch];
  if (!spec) throw new Error(`No QEMU architecture for board '${board}': ${arch ?? "<unset>"}`);
  return spec;
}

export function requireSignedInputs(image: string | undefined, certificate: string | undefined, append: string | undefined): void {
  if (!image || !certificate) throw new Error("MOS_QEMU_IMAGE and MOS_QEMU_BOOT_CERT are required");
  if (append !== undefined) throw new Error("Kernel command-line overrides are forbidden; seed DATA test units instead");
}

const REPO_ROOT = path.resolve(import.meta.dir, "../../../../..");
const MIB = 1048576;
function integer(value: string | undefined, fallback: number): number {
  const result = Number(value ?? fallback);
  if (!Number.isSafeInteger(result) || result <= 0) throw new Error(`Invalid positive integer: ${value}`);
  return result;
}

function innerRunSh(arch: QemuArch): string {
  return `set -euo pipefail
cd /w
if [ ! -f vars.fd ]; then
  virt-fw-vars --input ${arch.firmwareVars} --output vars.fd \\
    --set-pk 6b62601e-3448-4418-8923-7c9fa22ab09b db.cert.pem \\
    --add-kek 6b62601e-3448-4418-8923-7c9fa22ab09b db.cert.pem \\
    --add-db 6b62601e-3448-4418-8923-7c9fa22ab09b db.cert.pem --no-microsoft --sb
fi
( sleep "$RUN_SECONDS"
  printf 'system_powerdown\\n' | socat - UNIX-CONNECT:/run/mon.sock
  sleep 90
  printf 'quit\\n' | socat - UNIX-CONNECT:/run/mon.sock ) &
exec ${arch.binary} -machine ${arch.machine} -cpu max -m "$MEM" -smp 2 \\
  -monitor unix:/run/mon.sock,server,nowait -nographic -no-reboot \\
  -device i6300esb -watchdog-action reset \\
  -drive if=pflash,format=raw,unit=0,readonly=on,file=${arch.firmwareCode} \\
  -drive if=pflash,format=raw,unit=1,file=/w/vars.fd \\
  -drive if=none,id=disk0,format=raw,file=/w/disk.img \\
  -device virtio-blk-pci,drive=disk0,bootindex=0 \\
  -netdev user,id=net0$HOSTFWD -device virtio-net-pci,netdev=net0
`;
}

async function main(): Promise<void> {
  const env = process.env;
  requireSignedInputs(env.MOS_QEMU_IMAGE, env.MOS_QEMU_BOOT_CERT, env.MOS_QEMU_APPEND);
  const board = env.MOS_BOARD ?? "x64";
  if (board !== "x64" && board !== "virt-arm64") throw new Error("QEMU acceptance requires x64 or virt-arm64");
  const layout = parseFileLayout(fs.readFileSync(path.join(REPO_ROOT, "boards", board, "board.env"), "utf8"));
  if (layout.backend !== "systemd-boot") throw new Error("Expected the current signed UEFI layout");
  const arch = qemuArchFor(board === "x64" ? "amd64" : "arm64", board);
  const image = path.resolve(env.MOS_QEMU_IMAGE!);
  const certificate = path.resolve(env.MOS_QEMU_BOOT_CERT!);
  if (!fs.lstatSync(image).isFile() || !fs.lstatSync(certificate).isFile()) throw new Error("Boot inputs must be regular files");
  const runDir = path.join(REPO_ROOT, "_out", board, ".qemu");
  const disk = path.join(runDir, "disk.img");
  const [mode, ...args] = process.argv.slice(2);
  if (mode === "--seed") {
    const { files, enabled } = seedArguments(args);
    await seedDataImage(board, disk, files, enabled);
    return;
  }
  if (!(mode === "--prepare-only" && args.length === 0) && !(mode === "--capture" && args.length === 1)) {
    throw new Error("Usage: qemu.ts --prepare-only | --capture FILE | --seed SOURCE /state/TARGET ...");
  }
  if (env.MOS_QEMU_REUSE_DISK === "1") {
    if (!fs.existsSync(disk)) throw new Error("The prepared disk does not exist");
  } else {
    const diskMib = integer(env.MOS_QEMU_DISK_MIB, 4096);
    if (diskMib * MIB <= fs.statSync(image).size) throw new Error("Virtual medium must be larger than the factory image");
    fs.rmSync(runDir, { recursive: true, force: true });
    fs.mkdirSync(runDir, { recursive: true });
    const copied = spawnSync("cp", ["--reflink=auto", "--sparse=always", image, disk]);
    if (copied.status !== 0) throw new Error("Cannot copy the factory image");
    fs.truncateSync(disk, diskMib * MIB);
    fs.copyFileSync(certificate, path.join(runDir, "db.cert.pem"));
  }
  if (mode === "--prepare-only") {
    console.log(`Prepared ${disk}; seed DATA, then use MOS_QEMU_REUSE_DISK=1`);
    return;
  }
  fs.writeFileSync(path.join(runDir, "run.sh"), innerRunSh(arch));
  const name = `ai-agent-mos-api-${board}-${process.pid}`;
  const dockerArgs = ["run", "--rm", "--label", "ai-agent=true", "--name", name,
    "--network", env.MOS_QEMU_NETWORK ?? "traefik", "-v", `${runDir}:/w`,
    "-e", `MEM=${integer(env.MOS_QEMU_MEM, 2048)}`, "-e", `RUN_SECONDS=${integer(env.MOS_QEMU_RUN_SECONDS, 2400)}`];
  let hostfwd = "";
  if (env.MOS_QEMU_FORWARD === "1") {
    for (const [raw, fallback, guest] of [[env.MOS_QEMU_HTTPS_PORT, 18443, 443], [env.MOS_QEMU_HTTP_PORT, 18080, 80],
      ...(env.MOS_QEMU_SSH_PORT ? [[env.MOS_QEMU_SSH_PORT, 18022, 22] as const] : [])] as const) {
      const port = integer(raw, fallback);
      if (port > 65535) throw new Error("Invalid forwarding port");
      hostfwd += `,hostfwd=tcp::${port}-:${guest}`;
      dockerArgs.push("-p", `127.0.0.1:${port}:${port}`);
    }
  }
  dockerArgs.push("-e", `HOSTFWD=${hostfwd}`, "ai-agent/mos-p2-lab", "bash", "/w/run.sh");
  const capture = fs.openSync(args[0]!, "w");
  const stop = () => spawnSync("docker", ["stop", "--time", "10", name], { stdio: "ignore", timeout: 15000 });
  process.on("SIGTERM", stop);
  process.on("SIGINT", stop);
  try {
    const child = spawn("docker", dockerArgs, { stdio: ["ignore", capture, capture] });
    const backstop = setTimeout(stop, integer(env.MOS_QEMU_TIMEOUT, 2700) * 1000);
    try {
      const code = await new Promise<number | null>((resolve, reject) => {
        child.once("error", reject);
        child.once("close", resolve);
      });
      if (code !== 0) throw new Error(`QEMU container exited ${code}; see ${args[0]}`);
    } finally { clearTimeout(backstop); }
  } finally {
    stop();
    process.off("SIGTERM", stop);
    process.off("SIGINT", stop);
    fs.closeSync(capture);
  }
}

if (import.meta.main) await main();
