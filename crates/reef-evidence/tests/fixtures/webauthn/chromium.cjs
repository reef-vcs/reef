// Regenerates chromium-es256.json and chromium-eddsa.json: real browser bytes
// for tests/presence.rs, from Chromium's CDP virtual authenticator.
// Usage: PW=<path to node_modules/playwright> node chromium.cjs <output dir>
const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const path = require("path");
const pw = require(process.env.PW);

const keyId = (seed) => {
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), Buffer.alloc(32, seed)]);
  const key = crypto.createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  const spki = crypto.createPublicKey(key).export({ format: "der", type: "spki" });
  return crypto.createHash("sha256").update(spki).digest("hex");
};
const pae = (type, body) =>
  Buffer.concat([Buffer.from(`DSSEv1 ${type.length} ${type} ${body.length} `), body]);

const inputs = {
  authority: keyId(0xa1),
  key: keyId(0xd4),
  identity: "idn_8f3a0c2d91e4b675",
  session: "ses_41c9e07ab2d35f18",
  statement: crypto.createHash("sha256").update("a signoff statement").digest("hex"),
  change: "8d1f0e7c6b5a49382716f5e4d3c2b1a0",
};
const payload = Buffer.from(
  `reef-presence-approval/v1\n${inputs.authority}\n${inputs.key}\n${inputs.identity}\nhuman\n${inputs.session}\n${inputs.statement}\n${inputs.change}\n`,
);
const challenge = crypto
  .createHash("sha256")
  .update(pae("application/vnd.reef-vcs.presence-approval", payload))
  .digest();

const server = http.createServer((_, res) => {
  res.writeHead(200, { "content-type": "text/html" });
  res.end("<!doctype html><title>vectors</title>");
});
server.listen(0, "127.0.0.1", async () => {
  const origin = `http://localhost:${server.address().port}`;
  const browser = await pw.chromium.launch();
  for (const [name, alg] of [["es256", -7], ["eddsa", -8]]) {
    const context = await browser.newContext();
    const page = await context.newPage();
    const cdp = await context.newCDPSession(page);
    await cdp.send("WebAuthn.enable");
    await cdp.send("WebAuthn.addVirtualAuthenticator", {
      options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true },
    });
    await page.goto(`${origin}/`);
    const out = await page.evaluate(async ({ alg, challenge }) => {
      const b64 = (buffer) => btoa(String.fromCharCode(...new Uint8Array(buffer)));
      const created = await navigator.credentials.create({ publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)), rp: { name: "Reef" },
        user: { id: new Uint8Array([1]), name: "Reef: alice", displayName: "Reef: alice" },
        pubKeyCredParams: [{ type: "public-key", alg }],
        authenticatorSelection: { userVerification: "required" }, attestation: "none" } });
      const asserted = await navigator.credentials.get({ publicKey: {
        challenge: new Uint8Array(challenge), rpId: "localhost",
        allowCredentials: [{ type: "public-key", id: created.rawId }], userVerification: "required" } });
      return {
        credentialId: b64(created.rawId),
        publicKey: b64(created.response.getPublicKey()),
        algorithm: created.response.getPublicKeyAlgorithm(),
        authenticatorData: b64(asserted.response.authenticatorData),
        clientDataJSON: b64(asserted.response.clientDataJSON),
        signature: b64(asserted.response.signature),
      };
    }, { alg, challenge: [...challenge] });
    const vector = { browser: `chromium ${browser.version()}`, origin, rpId: "localhost", inputs, challenge: challenge.toString("base64url"), ...out };
    fs.writeFileSync(path.join(process.argv[2], `chromium-${name}.json`), JSON.stringify(vector, null, 2) + "\n");
    await context.close();
  }
  await browser.close();
  server.close();
});
