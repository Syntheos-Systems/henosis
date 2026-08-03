/** Black-box proof of Henosis conversation traffic through compiled Tauri and live Rift. */

import assert from "node:assert/strict";

/** Read one required E2E value without including its contents in failures. */
function requireEnvironment(name) {
  const value = process.env[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} is required by the live E2E harness`);
  }
  return value;
}

const RIFT_ENDPOINT = requireEnvironment("HENOSIS_E2E_RIFT_URL");
const USERNAME = requireEnvironment("HENOSIS_E2E_USERNAME");
const PASSWORD = requireEnvironment("HENOSIS_E2E_PASSWORD");
const EMAIL = requireEnvironment("HENOSIS_E2E_EMAIL");
const SERVER_NAME = requireEnvironment("HENOSIS_E2E_SERVER_NAME");
const SEED_MESSAGE = requireEnvironment("HENOSIS_E2E_SEED_MESSAGE");
const LIVE_MESSAGE = requireEnvironment("HENOSIS_E2E_LIVE_MESSAGE");
const UI_MESSAGE = requireEnvironment("HENOSIS_E2E_UI_MESSAGE");

/** Reduce a non-sensitive Rift error response to one bounded diagnostic. */
function safeResponseDetail(body) {
  return body.replace(/[\u0000-\u001f\u007f]+/g, " ").slice(0, 300);
}

/** Call one public Rift endpoint without ever logging authorization material. */
async function requestRift(
  path,
  { method = "GET", token, body, sensitiveResponse = false } = {},
) {
  const headers = { Accept: "application/json" };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  if (token !== undefined) {
    headers.Authorization = `Bearer ${token}`;
  }

  const response = await fetch(new URL(path, `${RIFT_ENDPOINT}/`), {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  const responseBody = await response.text();
  if (!response.ok) {
    const detail = sensitiveResponse ? "" : `: ${safeResponseDetail(responseBody)}`;
    throw new Error(`Rift ${method} ${path} returned HTTP ${response.status}${detail}`);
  }
  if (responseBody.length === 0) {
    return null;
  }
  try {
    return JSON.parse(responseBody);
  } catch {
    throw new Error(`Rift ${method} ${path} returned invalid JSON`);
  }
}

/** Report whether WebDriver is controlling a real Tauri webview. */
function detectTauriRuntime() {
  return Boolean(window.__TAURI_INTERNALS__);
}

/** Seed one disposable Rift identity, server, room, and initial message. */
async function seedRift() {
  const authentication = await requestRift("api/auth/register", {
    method: "POST",
    sensitiveResponse: true,
    body: {
      username: USERNAME,
      email: EMAIL,
      password: PASSWORD,
      display_name: "Henosis E2E",
    },
  });
  assert.equal(typeof authentication?.token, "string");
  assert.ok(authentication.token.length > 0);

  const server = await requestRift("api/servers", {
    method: "POST",
    token: authentication.token,
    body: { name: SERVER_NAME, description: "Live desktop E2E" },
  });
  assert.ok(Array.isArray(server?.channels));
  assert.equal(server.channels.length, 1);
  const channelId = server.channels[0]?.id;
  assert.equal(typeof channelId, "string");

  await createMessage(channelId, authentication.token, SEED_MESSAGE);
  return { token: authentication.token, channelId };
}

/** Create one message through Rift's public authenticated HTTP API. */
async function createMessage(channelId, token, content) {
  const message = await requestRift(`api/channels/${channelId}/messages`, {
    method: "POST",
    token,
    body: { content, attachment_ids: [], message_type: null },
  });
  assert.equal(message?.content, content);
  return message;
}

/** Read the room timeline text through its stable accessibility contract. */
async function timelineText(timeline) {
  return timeline.getText();
}

/** Prove the visible UI uses the native client for live inbound and outbound traffic. */
async function proveLiveConversation() {
  const { token, channelId } = await seedRift();
  assert.equal(await browser.execute(detectTauriRuntime), true);

  const endpointInput = await $("#rift-endpoint");
  await endpointInput.waitForDisplayed();
  await endpointInput.setValue(RIFT_ENDPOINT);
  await $("#rift-username").setValue(USERNAME);
  await $("#rift-password").setValue(PASSWORD);

  const connectButton = await $("button=Connect and view rooms");
  await connectButton.waitForClickable();
  await connectButton.click();

  const continueButton = await $("button=Continue room");
  await continueButton.waitForClickable();
  await continueButton.click();

  const conversation = await $("[aria-label='Room conversation']");
  await conversation.waitForDisplayed();
  const connectionStatus = await $("[aria-label='Connection status']");
  await browser.waitUntil(
    async () => (await connectionStatus.getText()) === "Connected",
    {
      timeout: 20_000,
      interval: 200,
      timeoutMsg: "native room gateway never reached Connected",
    },
  );

  const timeline = await $("[role='log'][aria-label='Room message timeline']");
  await timeline.waitForDisplayed();
  await browser.waitUntil(
    async () => (await timelineText(timeline)).includes(SEED_MESSAGE),
    {
      timeout: 10_000,
      interval: 200,
      timeoutMsg: "seeded Rift snapshot message never appeared",
    },
  );

  await createMessage(channelId, token, LIVE_MESSAGE);
  await browser.waitUntil(
    async () => (await timelineText(timeline)).includes(LIVE_MESSAGE),
    {
      timeout: 15_000,
      interval: 200,
      timeoutMsg: "post-connect Rift message never arrived without refresh",
    },
  );

  const composer = await $("[aria-label='Message Rift room']");
  await composer.waitForEnabled();
  await composer.setValue(UI_MESSAGE);
  const sendButton = await $("button=Send message");
  await sendButton.waitForClickable();
  await sendButton.click();
  await browser.waitUntil(
    async () => (await timelineText(timeline)).includes(UI_MESSAGE),
    {
      timeout: 10_000,
      interval: 200,
      timeoutMsg: "UI-authored message never appeared in the native timeline",
    },
  );

  await browser.waitUntil(
    async () => {
      const messages = await requestRift(
        `api/channels/${channelId}/messages?after=${channelId}&limit=100`,
        { token },
      );
      return (
        Array.isArray(messages) &&
        messages.some((message) => message?.content === UI_MESSAGE)
      );
    },
    {
      timeout: 10_000,
      interval: 200,
      timeoutMsg: "UI-authored message never reached Rift persistence",
    },
  );
}

/** Register the single serialized live-conversation acceptance scenario. */
describe("compiled Henosis desktop with live Rift", function liveConversationSuite() {
  /** Exercise the native session, snapshot, WebSocket, event, and command paths. */
  it(
    "receives a post-connect message without refresh and sends through the UI",
    proveLiveConversation,
  );
});
