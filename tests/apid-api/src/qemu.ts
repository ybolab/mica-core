/**
 * Boot a UEFI board's disk image in QEMU, through firmware, from the disk.
 *
 *   bun run src/qemu.ts --prepare-only
 *   MOS_QEMU_TIMEOUT=300 bun run src/qemu.ts --capture <file>
 *
 * Not `-kernel`. QEMU will happily load a kernel and initrd from the host and
 * skip the disk entirely, and that would be a faster test of a smaller thing:
 * it bypasses the firmware, GRUB, grubenv and the A/B order, which on this
 * board are exactly the parts with no other test. The image here boots the way
 * a real machine boots it: the firmware finds the ESP, runs the removable-media
 * EFI binary, GRUB reads
 * its own grub.cfg and grubenv, and the kernel comes off the same partition
 * RAUC updates. QEMU runs inside a container because this host has none, and
 * because pinning the machine model, the firmware build and the disk interface
 * here means a green run means the same thing on someone else's laptop.
 *
 * WHERE THIS RUNS. It drives `docker run` and it is TypeScript, so it needs bun
 * and a docker client in one place. `pkgs/mosd/tests/apid-api/run.sh` gives it both: the
 * bun pinned as IMAGE_BUN_1 plus the client pinned as IMAGE_DOCKER_CLI_28, the
 * image verify/Dockerfile assembles, with the daemon socket mounted and the
 * repository mounted at its own path. Its own path and not /w, because every
 * `docker run` below hands the daemon a path: the daemon is the host's, the
 * containers it makes are siblings rather than children, and a path that means
 * something here has to mean the same thing there. There is no prefix rewrite
 * to get wrong because there is no prefix.
 *
 * The two modes are the two this harness uses. The former shell tool also
 * booted with the console attached when given no argument; nothing called it
 * that way, and a console attached to a process that is itself inside a
 * container is not a thing to keep on the strength of a usage line.
 *
 * The exported functions above main() are pure and are driven by
 * src/selftest.ts against wrong inputs -- the four measured defects
 * found in the shell original are negative fixtures there, and they need no
 * docker, no QEMU and no image because the logic they cover is text.
 */
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

const MIB = 1048576;

/** Where GRUB's configuration sits on the ESP, in mtools' notation. */
export const ESP_GRUB_CFG = "::/EFI/mos/grub.cfg";

// --- which machine, which firmware, which emulator --------------------------

/**
 * The per-architecture facts of booting a mos UEFI board under QEMU.
 *
 * A TABLE, and only four fields, because that is the entire architecture
 * dependence of this harness. Everything else it does -- the ESP offset, the
 * grub.cfg rewrite, the graceful shutdown, the disk copy -- is board-generic
 * already or is read from the board definition.
 *
 * SYSTEM EMULATION, NOT binfmt. `qemu-system-aarch64` is an ordinary amd64 ELF
 * that emulates an aarch64 MACHINE: the host kernel never execs aarch64 code,
 * so binfmt_misc is not consulted and the `mos-arm64` buildx builder is not
 * involved. That is a different mechanism from `docker run --platform
 * linux/arm64`, which is USER-MODE emulation and which this host cannot do at
 * all. Both packages below are `Architecture: amd64` or `all` and install into
 * the same pinned Debian base as the x86 pair.
 *
 * The machine model is the bare alias on both -- `q35`, `virt` -- rather than a
 * versioned one. A versioned model would pin the machine to a QEMU release that
 * the apt line above it does not pin, which reads stronger than it is.
 */
interface QemuArch {
  /** The system emulator binary. */
  readonly binary: string;
  /** The `-machine` model. */
  readonly machine: string;
  /** The apt packages that provide the emulator and its firmware. */
  readonly packages: string;
  /** The pflash pair: UEFI code, and the writable variable store. */
  readonly firmwareCode: string;
  readonly firmwareVars: string;
}

export const QEMU_ARCHES: Readonly<Record<string, QemuArch>> = {
  amd64: {
    binary: "qemu-system-x86_64",
    machine: "q35",
    packages: "qemu-system-x86 ovmf",
    firmwareCode: "/usr/share/OVMF/OVMF_CODE_4M.fd",
    firmwareVars: "/usr/share/OVMF/OVMF_VARS_4M.fd",
  },
  arm64: {
    binary: "qemu-system-aarch64",
    machine: "virt",
    // qemu-system-arm provides qemu-system-aarch64; qemu-efi-aarch64 is
    // `Architecture: all` and provides AAVMF, Debian's EDK2 build for aarch64.
    packages: "qemu-system-arm qemu-efi-aarch64",
    // AAVMF_CODE.fd is a symlink to the no-secboot variant, and that is the one
    // wanted: the secure-boot builds expect an enrolled key this project does
    // not have, and docs/design/security-model.md section 4 already places
    // firmware verification with the platform owner rather than with mos.
    //
    // These files are 64 MiB, where OVMF's 4M pair is 4 MiB. The size is the
    // flash device's, not a preference: a pflash drive whose backing file is
    // not the device size does not boot.
    firmwareCode: "/usr/share/AAVMF/AAVMF_CODE.fd",
    firmwareVars: "/usr/share/AAVMF/AAVMF_VARS.fd",
  },
};

/** The arch row for a board, refusing a board whose architecture has none. */
export function qemuArchFor(arch: string | undefined, board: string): QemuArch {
  const spec = arch === undefined ? undefined : QEMU_ARCHES[arch];
  if (spec === undefined) {
    throw new Error(
      `board '${board}' declares MOS_ARCH=${JSON.stringify(arch ?? "<unset>")}, which this harness ` +
        `has no emulator, machine model or firmware for; known: ${Object.keys(QEMU_ARCHES).sort().join(", ")}`,
    );
  }
  return spec;
}

// --- the board layout, and the offset the ESP starts at ---------------------

/**
 * `boards/<board>/board.env` as a map of its literal assignments.
 *
 * Only literal assignments are read. That file also carries `$((...))`
 * arithmetic over the keys above it -- `ESP_OFFSET_BYTES=$((ESP_START_MIB *
 * MIB_BYTES))` and forty-odd more -- and nothing here needs a value that has to
 * be evaluated. A half-evaluator that got one of those wrong would be worse
 * than not having one, because it would answer.
 */
export function readBoardEnv(text: string): Map<string, string> {
  const env = new Map<string, string>();
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq);
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) continue;
    let value = line.slice(eq + 1);
    if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) {
      value = value.slice(1, -1);
    }
    env.set(key, value);
  }
  return env;
}

/**
 * The byte offset the ESP starts at, which is where mtools is pointed.
 *
 * ESP_START_MIB, not BOOT_A_START_MIB. The two are different partitions on
 * this board and only one of them holds a grub.cfg: the ESP (partition 1, at
 * 1 MiB) carries EFI/mos/grub.cfg and EFI/mos/grubenv, while BOOT-A
 * (partition 2, at 65 MiB) carries vmlinuz and cmdline.cfg at its FAT root and
 * has no EFI directory at all. It carried an initrd.img too until PLAN-074;
 * this board's kernel assembles the dm-verity root from the command line now.
 * Measured against
 * x64-mos-latest.img, 2026-08-28. Reading the boot slot here made mcopy
 * fail with `File "::/EFI/mos/grub.cfg" not found`, which took the whole
 * prepare down -- and since the apid-api harness always sets MOS_QEMU_APPEND
 * (its readiness signal is the journald line the append produces), that
 * failure was unconditional.
 */
export function espOffsetBytes(env: ReadonlyMap<string, string>): number {
  const raw = env.get("ESP_START_MIB");
  if (raw === undefined || !/^[0-9]+$/.test(raw)) {
    throw new Error(
      `the board layout defines no numeric ESP_START_MIB (read ${JSON.stringify(raw ?? "<unset>")}); ` +
        `${ESP_GRUB_CFG} is read at that offset and no default could be right`,
    );
  }
  return Number(raw) * MIB;
}

// --- the kernel append, applied to the disk copy's ESP grub.cfg -------------

/**
 * The indentation is matched as WHITESPACE, not as four spaces.
 * `boards/<board>/grub.cfg` indents both of its linux lines -- the A slot's and
 * the B slot's -- with EIGHT, and always has, so a four-space pattern matched
 * neither and the count came back 0 on every run.
 */
const LINUX_LINE = /^[ \t]*linux .*$/gm;

/** Zero linux lines: the append would land nowhere and the run would look normal. */
export class NoLinuxLineError extends Error {}

export interface AppendResult {
  /** The rewritten configuration, or the input unchanged when it was already there. */
  readonly text: string;
  /** False when the append was already on the line. */
  readonly applied: boolean;
  /** How many linux lines were matched -- both slots, on this board. */
  readonly matched: number;
}

/**
 * Add `append` to every linux line of an ESP grub.cfg, once.
 *
 * The guard is reachable, which in the shell original it was not: `before=$(grep
 * -c ...)` under `set -e` aborted the shell on a count of zero, BEFORE the next
 * line's `[ "${before}" -ge 1 ] || echo "error: ..."` could say why, so a
 * completely unmatched pattern presented as a prepare step that printed nothing
 * and failed. There is no such trap here, but the refusal is what carries the
 * reason and it is asserted in the selftest rather than assumed.
 *
 * Already there: leave it alone. This runs on the prepare AND on every boot
 * that reuses the disk, so an unconditional append lands the same arguments two
 * or three times over a run. For `systemd.journald.forward_to_console=1` a
 * repeat is the same value twice and costs nothing; for `systemd.run=` it is
 * not, because systemd takes each occurrence as another ExecStart and RUNS THE
 * COMMAND AGAIN. Measured 2026-08-28: a duplicated systemd.run executed the
 * seeded script twice, back to back, on one boot.
 *
 * `String.includes` and not a pattern: the append is a fixed string, and a
 * value carrying a `.` or a `*` must be looked for as itself rather than as a
 * pattern that happens to match something else on the line.
 */
export function applyKernelAppend(text: string, append: string): AppendResult {
  const matched = text.match(LINUX_LINE)?.length ?? 0;
  if (matched === 0) {
    throw new NoLinuxLineError(
      `no linux line in the ESP grub.cfg (${ESP_GRUB_CFG}); MOS_QEMU_APPEND would have added ` +
        `nothing and the run would look normal`,
    );
  }
  if (text.includes(append)) return { text, applied: false, matched };
  // A replacer function, not a `$1` string: `$&` and `$'` in a kernel command
  // line would otherwise be read as replacement syntax rather than as bytes.
  return { text: text.replace(LINUX_LINE, (line) => `${line} ${append}`), applied: true, matched };
}

// --- everything below runs; nothing above it does ---------------------------

const HERE = import.meta.dir;
const REPO_ROOT = path.resolve(HERE, "../../../../..");

/**
 * Which board is being booted.
 *
 * From `MOS_BOARD`, and it has a DEFAULT of x64 where the assembler
 * deliberately has none. The two are different questions: the assembler is
 * asked to produce an artefact and a wrong board there writes one board's
 * layout under another's name, silently. This is asked to boot an artefact
 * that already exists, names it in every message, and dies immediately if it
 * is not there -- and it is invoked by run.sh, which resolves MOS_BOARD once
 * and passes the same value to every step.
 */
const BOARD = process.env.MOS_BOARD ?? "x64";
const OUT_DIR = path.join(REPO_ROOT, "_out", BOARD);
const RUN_DIR = path.join(OUT_DIR, ".qemu");

function note(message: string): void {
  console.log(message);
}

function die(message: string): never {
  console.error(message);
  process.exit(1);
}

/** `docker run`, inherited stdio, with the exit status as the answer. */
function docker(args: readonly string[]): number {
  const result = spawnSync("docker", args, { stdio: "inherit" });
  if (result.error !== undefined) die(`error: could not run docker: ${String(result.error)}`);
  return result.status ?? 1;
}

/**
 * The container everything here runs in, resolved from build-env/images.env
 * through the one resolver. Unlike the assemblers, this is resolved up front
 * and not inside a branch, because there is no host path to protect: QEMU runs
 * in a container because this host has none, so every route through this file
 * needs the image and a failure to resolve it fails the run either way.
 *
 * One resolution for three uses -- the two mtools steps that rewrite the ESP
 * grub.cfg under MOS_QEMU_APPEND, and the qemu invocation at the bottom. They
 * were three separate `debian:trixie-slim` literals once and had to stay in
 * step by hand; the machine model, the firmware build and the disk interface
 * are pinned precisely so "a green run means the same thing on someone else's
 * laptop", and the ovmf that supplies the firmware comes out of this base.
 */
function resolveQemuImage(): string {
  const result = spawnSync("bash", [path.join(REPO_ROOT, "build-env/from.sh"), "--ref", "IMAGE_DEBIAN_TRIXIE"], {
    encoding: "utf8",
  });
  if (result.status !== 0) {
    die(
      `error: build-env/from.sh could not resolve IMAGE_DEBIAN_TRIXIE:\n${(result.stderr ?? "").trim()}`,
    );
  }
  return (result.stdout ?? "").trim();
}

/**
 * The script the QEMU container runs, written into the run directory.
 *
 * ovmf is Debian's build of the EDK2 UEFI firmware. The vars file is writable
 * and per-run: UEFI stores its boot order there, and a shared one would carry a
 * previous run's decisions into this one.
 *
 * A GRACEFUL SHUTDOWN, not a kill. The first version let `timeout` send
 * SIGTERM to qemu, which stops the machine where it stands: the guest's page
 * cache is never written, so /var/log/journal was EMPTY on a disk whose
 * systemd-journal-flush.service had reported success. Nothing was wrong with
 * the guest -- the harness threw the evidence away.
 *
 * MOS_QEMU_RUN_SECONDS of running, then ACPI power button, which systemd turns
 * into a real shutdown: units stop, filesystems unmount, and what the device
 * wrote is on the disk. It also means the shutdown path itself is exercised
 * rather than skipped.
 */
const innerRunSh = (a: QemuArch): string => `set -eu
RUN_SECONDS="\${RUN_SECONDS:-}"
# Empty unless MOS_QEMU_FORWARD asked for it; \`set -u\` would kill the run
# otherwise, after the image had already been copied and grown.
HOSTFWD="\${HOSTFWD:-}"
cp ${a.firmwareCode} /run/code.fd
cp ${a.firmwareVars} /run/vars.fd

if [ -n "\${RUN_SECONDS}" ]; then
    ( sleep "\${RUN_SECONDS}"
      if ! printf 'system_powerdown\\n' | socat - UNIX-CONNECT:/run/mon.sock >/dev/null; then
          echo "error: could not reach the QEMU monitor to request shutdown" >&2
      fi
      # A guest that ignores the power button must not hold the harness open
      # forever; 90s after the request, take it down.
      sleep 90
      echo "note: guest did not power off 90s after the request; taking it down" >&2
      printf 'quit\\n' | socat - UNIX-CONNECT:/run/mon.sock >/dev/null || true ) &
fi

exec ${a.binary} \\
    -monitor unix:/run/mon.sock,server,nowait \\
    -machine ${a.machine} \\
    -cpu max \\
    -m "\${MEM}" \\
    -nographic \\
    -no-reboot \\
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/run/code.fd \\
    -drive if=pflash,format=raw,unit=1,format=raw,file=/run/vars.fd \\
    -drive if=none,id=disk0,format=raw,file=/w/disk.img \\
    -device virtio-blk-pci,drive=disk0,bootindex=0 \\
    -netdev user,id=net0\${HOSTFWD} \\
    -device virtio-net-pci,netdev=net0 \\
    -serial mon:stdio
`;

/**
 * apt-get, then the run script. `timeout` wraps the docker client directly and
 * this is its whole command: an earlier shell version put the invocation in a
 * function and re-declared it into `bash -c` so timeout could see it, which
 * silently dropped the argument array -- a bash array does not survive being
 * exported. The container then ran with no volume and reported
 * "/w/run.sh: No such file or directory", a message about the script rather
 * than about the mount that was missing.
 */
const installAndRun = (a: QemuArch): string => `
    apt-get update -qq >/dev/null 2>&1
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \\
        ${a.packages} socat >/dev/null 2>&1
    command -v socat >/dev/null || { echo "error: socat is not installed; the graceful-shutdown request could not be sent and the run would end in a SIGTERM that discards whatever the guest had not yet written" >&2; exit 1; }
    bash /w/run.sh`;

/** mtools, installed once per container start, then one mcopy. */
function mtoolsScript(body: string): string {
  return `
            set -eu
            apt-get update -qq >/dev/null 2>&1
            DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends mtools >/dev/null 2>&1
            cd /w
            ${body}`;
}

interface Options {
  readonly capture: string;
  readonly prepareOnly: boolean;
}

function parseArgs(argv: readonly string[]): Options {
  const usage = "usage: bun run src/qemu.ts [--capture <file>] [--prepare-only]";
  if (argv[0] === "--capture") {
    const file = argv[1];
    if (file === undefined || file === "") die(`${usage}\nerror: --capture takes a file`);
    return { capture: file, prepareOnly: false };
  }
  if (argv[0] === "--prepare-only") return { capture: "", prepareOnly: true };
  die(`${usage}\nerror: ${argv.length === 0 ? "no mode given" : `unknown argument ${JSON.stringify(argv[0])}`}`);
}

/**
 * MOS_QEMU_APPEND adds kernel arguments to the disk copy, by rewriting the
 * grub.cfg in its ESP. The shipped image is untouched, and the boot still goes
 * through GRUB reading its own configuration, so this is a debugging knob and
 * not a second boot path. It exists because mos keeps the journal in RAM
 * (/etc/systemd/journald.conf.d/00-volatile.conf sets Storage=volatile, which
 * follows from /var being the EPHEMERAL partition). That is deliberate, and it
 * means a failed unit's reason is in a journal that dies with the machine: the
 * console shows "[FAILED] ... See systemctl status for details" and the details
 * are unreachable. systemd.journald.forward_to_console=1 puts them on the
 * serial line.
 *
 * Two container starts where the shell had one, and only when the append is not
 * already there: mcopy reads the file out, the rewrite happens here where it is
 * testable, and mcopy writes it back. A reuse boot -- where the append landed on
 * the prepare and this is a no-op -- pays the first start only.
 */
function applyAppendToDisk(qemuImage: string, offset: number, append: string): void {
  const local = path.join(RUN_DIR, "grub.cfg");
  const read = docker([
    "run", "--rm", "-v", `${RUN_DIR}:/w`, "-e", `OFF=${offset}`, qemuImage,
    "bash", "-c", mtoolsScript(`mcopy -n -i "disk.img@@\${OFF}" ${ESP_GRUB_CFG} grub.cfg`),
  ]);
  if (read !== 0) {
    die(
      `error: could not read ${ESP_GRUB_CFG} out of the disk copy at offset ${offset} ` +
        `(${offset / MIB} MiB, the ESP). MOS_QEMU_APPEND is set, so the run stops here rather ` +
        `than booting a disk that never received it.`,
    );
  }

  let result: AppendResult;
  try {
    result = applyKernelAppend(fs.readFileSync(local, "utf8"), append);
  } catch (error) {
    if (error instanceof NoLinuxLineError) die(`error: ${error.message}`);
    throw error;
  }
  if (!result.applied) {
    console.error("note: the append is already on the linux line; not adding it a second time");
    return;
  }
  fs.writeFileSync(local, result.text);

  const write = docker([
    "run", "--rm", "-v", `${RUN_DIR}:/w`, "-e", `OFF=${offset}`, qemuImage,
    "bash", "-c", mtoolsScript(`mcopy -o -n -i "disk.img@@\${OFF}" grub.cfg ${ESP_GRUB_CFG}`),
  ]);
  if (write !== 0) die(`error: the append landed in grub.cfg but mcopy could not write it back to the ESP`);
  note(`note: appended to the disk copy's kernel command line, on ${result.matched} linux line(s): ${append}`);
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const env = process.env;
  const boardEnvPath = path.join(REPO_ROOT, "boards", BOARD, "board.env");
  if (!fs.existsSync(boardEnvPath)) {
    die(`error: ${boardEnvPath} does not exist, so MOS_BOARD=${BOARD} names no board in this tree`);
  }
  const board = readBoardEnv(fs.readFileSync(boardEnvPath, "utf8"));
  // The machine, the emulator and the firmware, from the board's own arch.
  const arch = qemuArchFor(board.get("MOS_ARCH"), BOARD);
  const qemuImage = resolveQemuImage();
  const img = env.MOS_QEMU_IMAGE ?? path.join(OUT_DIR, board.get("IMAGE_LATEST_NAME") ?? "");
  const timeout = Number(env.MOS_QEMU_TIMEOUT ?? "240");
  const mem = env.MOS_QEMU_MEM ?? "2048";
  const reuseDisk = env.MOS_QEMU_REUSE_DISK === "1";
  const disk = path.join(RUN_DIR, "disk.img");

  if (!fs.existsSync(img)) {
    die(
      `error: ${img} not found. Build it: MOS_BOARD=${BOARD} bash rootfs/build.sh && ` +
        `bash build/run.sh --mkimage-uefi --board ${BOARD}`,
    );
  }

  // The image is never bound into the QEMU container; a copy of it under
  // _out/<board>/.qemu is, and that copy is what QEMU writes to -- so a run cannot
  // mutate the artefact it is testing, and repeated runs start from the same
  // state rather than from whatever the last one left. It is copied under _out/
  // and not through a temporary directory because it is ~2 GiB and because a
  // bind of a /tmp path does not propagate to the daemon on this host anyway
  // (see build/src/mkimage-uefi.ts).
  //
  // MOS_QEMU_REUSE_DISK keeps the disk a previous --prepare-only made, so
  // tools/qemu-seed-state.sh's writes survive into the boot. Without it every
  // run starts from the pristine image, which is the right default: a test that
  // silently inherited the last run's state would pass for reasons nobody chose.
  if (reuseDisk) {
    if (!fs.existsSync(disk)) {
      die(`error: MOS_QEMU_REUSE_DISK=1 but ${disk} does not exist; run --prepare-only first`);
    }
    note("note: reusing the prepared disk, including anything seeded into it");
    // Already grown by the --prepare-only run, and re-checking would compare
    // the disk against itself: the growth guard would then refuse a disk that
    // is exactly the size it was asked to be.
    note("note: disk was sized by the prepare step; not resizing");
  } else {
    fs.rmSync(RUN_DIR, { recursive: true, force: true });
    fs.mkdirSync(RUN_DIR, { recursive: true });
    fs.copyFileSync(fs.realpathSync(img), disk);

    // The virtual disk is made LARGER than the image, because that is what
    // flashing one to a device does. systemd-repart's job is to extend DATA into
    // whatever space the medium has beyond the image, and a disk that is exactly
    // the image gives it nothing to extend into -- repart then FAILS, which is
    // what the first x64 boot showed. Booting the image at its own size tests a
    // case no real device is ever in.
    const diskMib = Number(env.MOS_QEMU_DISK_MIB ?? "4096");
    const imgMib = Math.floor(fs.statSync(disk).size / MIB);
    if (diskMib <= imgMib) {
      die(
        `error: MOS_QEMU_DISK_MIB=${diskMib} is not larger than the ${imgMib} MiB image; ` +
          `systemd-repart would have nothing to grow into and the run would not exercise ` +
          `first-boot growth at all`,
      );
    }
    fs.truncateSync(disk, diskMib * MIB);
    note(`note: virtual disk is ${diskMib} MiB for a ${imgMib} MiB image, so systemd-repart has room to extend DATA`);
  }

  // Applied on both paths, reused disk included. Inside the "grow the disk"
  // branch it would only run when the disk is fresh, so every run that reused a
  // seeded disk would silently boot without the debugging arguments it was told
  // to add and mosd's journal would never reach the console -- a daemon that
  // looks silent and is not.
  const append = env.MOS_QEMU_APPEND ?? "";
  if (append !== "") applyAppendToDisk(qemuImage, espOffsetBytes(board), append);

  if (options.prepareOnly) {
    note(`prepared ${disk}; seed it with tools/qemu-seed-state.sh, then run with MOS_QEMU_REUSE_DISK=1`);
    return;
  }

  fs.writeFileSync(path.join(RUN_DIR, "run.sh"), innerRunSh(arch));

  const runSeconds = env.MOS_QEMU_RUN_SECONDS ?? "";
  const dockerArgs = ["run", "--rm", "-v", `${RUN_DIR}:/w`, "-e", `MEM=${mem}`, "-e", `RUN_SECONDS=${runSeconds}`];
  // /dev/kvm as this process sees it, which is what run.sh passes through with
  // `--device` when the machine it is on has one. The former shell tool made
  // the same test in its own filesystem namespace; this keeps the answer the
  // same rather than making it the runner image's.
  if (fs.existsSync("/dev/kvm")) dockerArgs.push("--device", "/dev/kvm");

  // MOS_QEMU_FORWARD opens a path from the host to apid inside the guest, for
  // the API suite. It is off by default, and the default is the point: a
  // management daemon is otherwise unreachable from outside the machine, which
  // is what makes an unattended run a closed box.
  //
  // Two doors, not one. QEMU's user-mode `hostfwd` binds inside the container,
  // so a forward alone reaches nothing; the container must publish the port
  // too. Getting one of the two right produces a connection refused with
  // nothing to say which half is missing, so both are set here or neither is.
  // The forward is bound to 127.0.0.1 on the host: the guest has no password
  // until the suite sets one, and until then anything that can reach the port
  // can complete first-boot setup and own the device.
  let hostfwd = "";
  if ((env.MOS_QEMU_FORWARD ?? "") !== "") {
    const httpsPort = env.MOS_QEMU_HTTPS_PORT ?? "18443";
    const httpPort = env.MOS_QEMU_HTTP_PORT ?? "18080";
    hostfwd = `,hostfwd=tcp::${httpsPort}-:443,hostfwd=tcp::${httpPort}-:80`;
    dockerArgs.push("-p", `127.0.0.1:${httpsPort}:${httpsPort}`, "-p", `127.0.0.1:${httpPort}:${httpPort}`);

    // The third door, and the one that is invisible until it bites.
    //
    // `-p 127.0.0.1:...` publishes on the DOCKER HOST's loopback. A caller
    // that is itself a container has its own loopback and its own network, so
    // it connects to itself and gets a refusal that says nothing about why.
    // Measured here: this repository's own session runs inside a container on
    // `traefik`/172.18.0.0/16 while a plain `docker run` lands on the default
    // bridge at 172.17.0.0/16, with no route between them. The publish was
    // correct, the hostfwd was correct, and the port was unreachable anyway.
    //
    // MOS_QEMU_NETWORK attaches the QEMU container to a named docker network so
    // a sibling container can reach it directly. The address to use is then the
    // CONTAINER's, not loopback, so it is printed rather than left to be
    // discovered.
    const network = env.MOS_QEMU_NETWORK ?? "";
    if (network !== "") {
      dockerArgs.push("--network", network);
      note(
        `note: QEMU joins the '${network}' network; a sibling container reaches it at ` +
          `<container-ip>:${httpsPort}, NOT at 127.0.0.1`,
      );
    }
    // MOS_QEMU_SSH_PORT, off unless asked for, because it is the way IN to a
    // machine whose whole security posture is that there is no way in until an
    // operator makes one. It exists because two things this repository has to
    // verify cannot be reached over HTTP at all: `rauc install`, which apid
    // exposes no endpoint for, and whether the transient root password actually
    // AUTHENTICATES -- the API can only report that it was set. A feature with
    // no way to exercise it end to end is the existence-versus-function trap in
    // its purest form, so the harness gets a door rather than the assertions
    // getting weaker.
    const sshPort = env.MOS_QEMU_SSH_PORT ?? "";
    if (sshPort !== "") {
      hostfwd = `${hostfwd},hostfwd=tcp::${sshPort}-:22`;
      dockerArgs.push("-p", `127.0.0.1:${sshPort}:${sshPort}`);
      note(
        `note: forwarding :${sshPort} -> guest :22; sshd still has to be enabled and given a key ` +
          `or a password through the API before it answers`,
      );
    }
    note(`note: forwarding :${httpsPort} -> guest :443 and :${httpPort} -> guest :80`);
    note(
      `note: on the docker host that is https://127.0.0.1:${httpsPort}; from another container it ` +
        `is the QEMU container's own address on a shared network (see MOS_QEMU_NETWORK)`,
    );
    note("note: the guest ships no admin password until something completes /api/v1/setup, which is why the host publish is loopback-only");
  }
  dockerArgs.push("-e", `HOSTFWD=${hostfwd}`);
  dockerArgs.push(qemuImage, "bash", "-c", installAndRun(arch));

  note(
    `note: booting ${path.basename(img)} on ${arch.binary} -machine ${arch.machine}; ` +
      "no /dev/kvm on this host means TCG, which is slow but complete",
  );

  // MOS_QEMU_TIMEOUT is when the container is given up on entirely. It is a
  // SIGTERM to the docker client, which is what `timeout docker run` was: the
  // container that outlives it is stopped by the caller, which knows it by the
  // run directory it binds.
  const handle = fs.openSync(options.capture, "w");
  try {
    const child = spawn("docker", dockerArgs, { stdio: ["ignore", handle, handle] });
    const backstop = setTimeout(() => child.kill("SIGTERM"), timeout * 1000);
    await new Promise<void>((resolve) => child.once("close", () => resolve()));
    clearTimeout(backstop);
  } finally {
    fs.closeSync(handle);
  }
  const lines = fs.readFileSync(options.capture, "utf8").split("\n").length - 1;
  note(`console captured to ${options.capture} (${lines} lines)`);
}

if (import.meta.main) await main();
