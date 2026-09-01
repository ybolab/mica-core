/** Cookie and bearer management through /api, with retired form routes inert. */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";
import { BEARER_STATE, CSRF_STATE } from "./02-session.ts";

function parseObject(body: string): Record<string, JsonValue> | undefined {
  try {
    const value = JSON.parse(body) as JsonValue;
    return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function parseObjectArray(body: string): Record<string, JsonValue>[] | undefined {
  try {
    const value = JSON.parse(body) as JsonValue;
    if (!Array.isArray(value)) return undefined;
    const rows = value.map((item) =>
      typeof item === "object" && item !== null && !Array.isArray(item) ? item : undefined,
    );
    return rows.every((row) => row !== undefined) ? rows : undefined;
  } catch {
    return undefined;
  }
}

const phase: Phase = {
  id: "03-api-management",
  title: "all appliance reads and writes flow through the authenticated JSON API",
  assumes: "02 left an authenticated browser session plus its CSRF token and setup bearer in phase state",

  async run({ client, report, config, state }: PhaseContext): Promise<void> {
    const csrf = state.get(CSRF_STATE);
    const bearer = state.get(BEARER_STATE);
    if (typeof csrf !== "string" || typeof bearer !== "string") {
      report.fail("the management phase received both credentials from setup", `csrf=${typeof csrf}; bearer=${typeof bearer}`);
      return;
    }

    const ui = await client.get("/api/v1/ui");
    report.expectStatus(ui, 200, "GET /api/v1/ui reports UI selection through the API");
    report.expectJson(ui, { mode: "builtIn" }, "the factory image reports the built-in UI active", { subset: true });
    const uiStatus = parseObject(ui.body);
    report.check(
      uiStatus !== undefined && !("availableCustom" in uiStatus),
      "the factory UiStatus has no optional availableCustom candidate",
      `actual body: ${ui.body}`,
    );

    const uiWithoutCsrf = await client.request("PUT", "/api/v1/ui/active");
    report.expectStatus(uiWithoutCsrf, 403, "PUT /api/v1/ui/active without CSRF is refused");
    report.expectJson(
      uiWithoutCsrf,
      { error: { code: "csrf_invalid", source: "apid" } },
      "the custom UI selector uses the common CSRF envelope",
      { subset: true },
    );

    const unavailableUi = await client.request("PUT", "/api/v1/ui/active", {
      headers: { "X-CSRF-Token": csrf },
    });
    report.expectStatus(unavailableUi, 409, "PUT /api/v1/ui/active reports no retained custom UI");
    report.expectJson(
      unavailableUi,
      { error: { code: "custom_ui_unavailable", source: "apid" } },
      "the absent custom UI has a named conflict response",
      { subset: true },
    );

    const hostname = await client.get("/api/v1/settings/hostname");
    report.expectStatus(hostname, 200, "GET /api/v1/settings/hostname reads through the browser session");
    report.expectJson(hostname, config.hostnameTarget, "the setup hostname reads back through the API");

    const current = await client.get("/api/v1/settings/container.enabled");
    report.expectStatus(current, 200, "GET /api/v1/settings/container.enabled reads the container switch");
    const enabled = JSON.parse(current.body) as JsonValue;
    if (typeof enabled !== "boolean") {
      report.fail("the container setting is a boolean", `actual body: ${current.body}`);
      return;
    }

    const refused = await client.request("PUT", "/api/v1/settings/container.enabled", {
      body: JSON.stringify(enabled),
      contentType: "application/json",
    });
    report.expectStatus(refused, 403, "a cookie-authenticated settings PUT without CSRF is refused");
    report.expectJson(
      refused,
      { error: { code: "csrf_invalid", source: "apid" } },
      "the missing-CSRF refusal uses the API envelope",
      { subset: true },
    );

    const accepted = await client.request("PUT", "/api/v1/settings/container.enabled", {
      body: JSON.stringify(enabled),
      contentType: "application/json",
      headers: { "X-CSRF-Token": csrf },
    });
    report.expectStatus(accepted, 202, "the same settings PUT with CSRF is accepted");
    const acceptedBody = parseObject(accepted.body);
    const taskId = acceptedBody?.["taskId"];
    report.check(
      typeof taskId === "string" && taskId.length > 0,
      "the accepted settings write returns its \"taskId\"",
      `actual body: ${accepted.body}`,
    );
    if (typeof taskId === "string") {
      const task = await client.get(`/api/v1/tasks/${encodeURIComponent(taskId)}`);
      report.expectStatus(task, 200, "GET /api/v1/tasks/{id} exposes the accepted write");
    }

    const parallelPaths = ["container.enabled", "mqtt.enabled", "access.ssh.enabled"] as const;
    const parallelWrites = await Promise.all(
      parallelPaths.map((path) =>
        client.request("PUT", `/api/v1/settings/${path}`, {
          body: "true",
          contentType: "application/json",
          headers: { "X-CSRF-Token": csrf },
        }),
      ),
    );
    const parallelTaskIds = new Map<string, string>();
    for (const [index, response] of parallelWrites.entries()) {
      const path = parallelPaths[index];
      if (path === undefined) continue;
      report.expectStatus(response, 202, `parallel enable of ${path} is accepted`);
      const id = parseObject(response.body)?.["taskId"];
      report.check(
        typeof id === "string" && id.length > 0,
        `parallel enable of ${path} returns a task id`,
        `actual body: ${response.body}`,
      );
      if (typeof id === "string") parallelTaskIds.set(path, id);
    }
    report.check(
      new Set(parallelTaskIds.values()).size === parallelPaths.length,
      "parallel enables of independent settings create three distinct tasks",
      `tasks: ${JSON.stringify(Object.fromEntries(parallelTaskIds))}`,
    );

    let terminalTasks: Record<string, JsonValue>[] | undefined;
    await report.expectEventually(
      "the task list retains every parallel enable through terminal state",
      async () => {
        const response = await client.get("/api/v1/tasks");
        if (response.status !== 200) return false;
        const tasks = parseObjectArray(response.body);
        if (tasks === undefined) return false;
        const byId = new Map(tasks.map((task) => [task["id"], task]));
        const selected = [...parallelTaskIds.values()].map((id) => byId.get(id));
        if (selected.some((task) => task === undefined || task["status"] !== "finished")) return false;
        terminalTasks = selected as Record<string, JsonValue>[];
        return true;
      },
      { timeoutMs: 120_000, intervalMs: 500 },
    );

    if (terminalTasks !== undefined) {
      for (const task of terminalTasks) {
        const path = task["dotPath"];
        report.check(
          typeof path === "string" && parallelTaskIds.get(path) === task["id"] && task["outcome"] === "succeeded",
          `task list reports the parallel ${String(path)} enable as succeeded`,
          `actual task: ${JSON.stringify(task)}`,
        );
      }
    }
    for (const path of parallelPaths) {
      const setting = await client.get(`/api/v1/settings/${path}`);
      report.expectJson(setting, true, `parallel enable of ${path} persists true`);
    }

    const bearerRead = await client.get("/api/v1/settings/hostname", {
      sendCookies: false,
      headers: { Authorization: `Bearer ${bearer}` },
    });
    report.expectStatus(bearerRead, 200, "the setup bearer reads the same management API without CSRF");

    const anonymous = await client.get("/api/v1/ui", { sendCookies: false });
    report.expectStatus(anonymous, 401, "an API management read with no credential is refused");

    for (const path of [
      "/containers/enable",
      "/mqtt/enable",
      "/hostname",
      "/network",
      "/ssh/enable",
      "/power/poweroff",
      "/builtin/deactivate",
    ] as const) {
      const response = await client.post(path, { enabled: "on", hostname: "must-not-apply" });
      report.expectStatus(response, 405, `POST ${path} remains outside the management surface after login`);
    }

    const unchanged = await client.get("/api/v1/settings/hostname");
    report.expectJson(unchanged, config.hostnameTarget, "retired form routes did not mutate the hostname");
  },
};

export default phase;
