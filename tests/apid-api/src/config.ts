/**
 * The environment contract for the apid black-box suite.
 *
 * Everything the suite needs to find the device comes through here, is
 * validated once, and is then frozen. Validation is loud and early on
 * purpose: a suite that starts, boots nothing, and fails on assertion 40
 * because a port was a typo costs a TCG boot to diagnose.
 */

/** A malformed or missing environment. Carries a message meant for a human. */
export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConfigError";
  }
}

export interface Config {
  /** Host or IP of the container running QEMU. Never loopback. See `loadConfig`. */
  readonly host: string;
  /** Published port that reaches the guest's apid HTTPS listener (guest :443). */
  readonly httpsPort: number;
  /** Published port that reaches the guest's apid HTTP listener (guest :80). */
  readonly httpPort: number;
  /** Path to the captured QEMU console log, if the caller captured one. */
  readonly consoleLog: string | undefined;
  /** Admin password the suite sets and reuses through the JSON session API. */
  readonly adminPassword: string;
  /** Hostname the setup phase assigns and the management phase reads back. */
  readonly hostnameTarget: string;
  /** Phase ids to run, or undefined for "all of them, in registry order". */
  readonly phases: readonly string[] | undefined;
  /** Where to write the machine-readable result, if anywhere. */
  readonly resultJson: string | undefined;
  /** See `Reporter`: inverts the verdict of the first matching check. */
  readonly negative: string | undefined;
}

type Env = Readonly<Record<string, string | undefined>>;

const DEFAULT_HTTPS_PORT = 18443;
const DEFAULT_HTTP_PORT = 18080;
const DEFAULT_ADMIN_PASSWORD = "mos-e2e-admin-pw";
const DEFAULT_HOSTNAME_TARGET = "mos-e2e-renamed";

function text(env: Env, name: string): string | undefined {
  const raw = env[name];
  if (raw === undefined) return undefined;
  const trimmed = raw.trim();
  return trimmed === "" ? undefined : trimmed;
}

function port(env: Env, name: string, fallback: number): number {
  const raw = text(env, name);
  if (raw === undefined) return fallback;
  const value = Number(raw);
  if (!Number.isInteger(value) || value < 1 || value > 65535) {
    throw new ConfigError(
      `${name} must be an integer TCP port in 1..65535; got ${JSON.stringify(raw)}`,
    );
  }
  return value;
}

/**
 * Read and validate the environment.
 *
 * `APID_HOST` has NO DEFAULT, and specifically no default of 127.0.0.1. The
 * guest is reached at the address of the *container running QEMU*, on the
 * docker network this session shares with it. There are three doors between a
 * caller and the guest -- QEMU's `hostfwd` binds inside that container, that
 * container must publish the port, and `-p 127.0.0.1:...` publishes on the
 * DOCKER HOST's loopback, which is not the caller's. A default of loopback
 * makes all three failures present identically as "connection refused", with
 * nothing in the message to say which door was shut.
 */
export function loadConfig(env: Env = process.env): Config {
  const host = text(env, "APID_HOST");
  if (host === undefined) {
    throw new ConfigError(
      "APID_HOST is not set. It must be the address of the CONTAINER RUNNING QEMU " +
        "on the shared docker network -- not 127.0.0.1, and not the docker host's " +
        "loopback. QEMU's hostfwd binds inside that container; loopback here reaches " +
        "neither the guest nor the forward.",
    );
  }
  if (host.includes("/")) {
    throw new ConfigError(
      `APID_HOST must be a bare host or IP, not a URL; got ${JSON.stringify(host)}`,
    );
  }

  const phasesRaw = text(env, "APID_PHASES");
  let phases: readonly string[] | undefined;
  if (phasesRaw !== undefined) {
    const ids = phasesRaw
      .split(",")
      .map((id) => id.trim())
      .filter((id) => id !== "");
    if (ids.length === 0) {
      throw new ConfigError(
        `APID_PHASES was set to ${JSON.stringify(phasesRaw)} but named no phases. ` +
          "Unset it to run every phase; an empty selection would run nothing and " +
          "still print a RESULT line.",
      );
    }
    phases = Object.freeze(ids);
  }

  return Object.freeze({
    host,
    httpsPort: port(env, "APID_HTTPS_PORT", DEFAULT_HTTPS_PORT),
    httpPort: port(env, "APID_HTTP_PORT", DEFAULT_HTTP_PORT),
    consoleLog: text(env, "APID_CONSOLE"),
    adminPassword: text(env, "APID_ADMIN_PASSWORD") ?? DEFAULT_ADMIN_PASSWORD,
    hostnameTarget: text(env, "APID_HOSTNAME_TARGET") ?? DEFAULT_HOSTNAME_TARGET,
    phases,
    resultJson: text(env, "APID_RESULT_JSON"),
    negative: text(env, "APID_NEGATIVE"),
  });
}

/**
 * `loadConfig`, but a bad environment ends the process instead of unwinding
 * into a stack trace. The suite's entry point wants the message, not the trace.
 */
export function loadConfigOrExit(env: Env = process.env): Config {
  try {
    return loadConfig(env);
  } catch (error) {
    const message = error instanceof ConfigError ? error.message : String(error);
    console.error(`FAIL: the environment is not usable: ${message}`);
    console.error("RESULT: FAIL (0/0 checks)");
    process.exit(2);
  }
}
