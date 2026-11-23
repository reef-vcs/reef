// `/passkeys` (R3.43): each person identity's passkeys, the `[passkeys]`
// lines that pin them in a repository's policy, and the ceremonies that add
// and remove them. A passkey verifies a person for a sign-off; a policy
// counts it only where it pins the fingerprint, and the local address the
// touch runs under (#346), which this page's is.
import { useEffect, useState, type SubmitEvent } from "react";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import {
  IdentityKind,
  IdentityService,
  type Identity,
  type Passkey,
} from "../gen/reef/api/v1/identity_pb";
import { equal, hex, sha256 } from "../sign/bytes";
import { removalChallenge } from "../sign/key";
import { assertionVerifies } from "../sign/verify";
import {
  assert,
  create,
  failure,
  hasPasskeySupport,
  NO_SUPPORT,
  type Failure,
} from "../sign/passkey";
import { device } from "../device";
import { embedded } from "../session";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { FailureView } from "../ui/FailureView";
import { dateTime } from "../ui/time";
import { NoPasskeysHere } from "./Handoff";

const PAGE = 1_000;

interface Person {
  identity: Identity;
  passkeys: Passkey[];
}

type Load =
  | { kind: "loading" }
  | { kind: "failed"; error: ReefError }
  /** `site`: what a policy pins for this page, empty when its port ends with the run. */
  | { kind: "loaded"; people: Person[]; site: string };

/** What one identity's last add or remove is doing, or how it ended. */
type Action =
  | { kind: "busy"; identity: string; words: string }
  | { kind: "done"; identity: string; words: string }
  | { kind: "failed"; identity: string; failure: Failure };

const TOML_BARE_KEY = /^[A-Za-z0-9_-]+$/;

/** The `[passkeys]` table pinning `person`'s passkeys at `origin`. */
export function policySnippet({ identity, passkeys }: Person, origin: string): string {
  const key = TOML_BARE_KEY.test(identity.name) ? identity.name : JSON.stringify(identity.name);
  const pins = passkeys.map(
    ({ fingerprint }) =>
      `  { fingerprint = ${JSON.stringify(fingerprint)}, origin = ${JSON.stringify(origin)} },\n`,
  );
  return `[passkeys]\n${key} = [\n${pins.join("")}]\n`;
}

function usePeople(connection: Connection, version: number, onSessionInvalid: () => void) {
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  useEffect(() => {
    let current = true;
    const identities = client(connection, IdentityService);
    Promise.all([
      identities.listIdentities({ pageSize: PAGE }),
      identities.listPasskeys({ pageSize: PAGE }),
      client(connection, DaemonService).getDaemonStatus({ skipBackends: true }),
    ]).then(
      ([listed, keys, daemon]) => {
        if (!current) return;
        const people = listed.identities
          .filter((identity) => identity.kind === IdentityKind.PERSON && !identity.revocation)
          .map((identity) => ({
            identity,
            passkeys: keys.passkeys.filter((passkey) => passkey.identityId === identity.id),
          }))
          .sort((a, b) => Number(b.identity.isDefault) - Number(a.identity.isDefault));
        setLoad({ kind: "loaded", people, site: daemon.webUi?.site ?? "" });
      },
      (thrown: unknown) => {
        if (!current) return;
        const error = reefError(thrown);
        if (isSessionInvalid(error)) onSessionInvalid();
        else setLoad({ kind: "failed", error });
      },
    );
    return () => {
      current = false;
    };
  }, [connection, version, onSessionInvalid]);
  return load;
}

function When({ at }: { at: Parameters<typeof dateTime>[0] }) {
  const { text, title } = dateTime(at);
  return <time title={title}>{text}</time>;
}

function PersonCard({
  person,
  site,
  action,
  busy,
  onAdd,
  onRemove,
}: {
  person: Person;
  site: string;
  action: Action | null;
  busy: boolean;
  onAdd: (label: string) => void;
  onRemove: (passkey: Passkey) => void;
}) {
  const { identity, passkeys } = person;
  const paired = device() !== null;
  const [label, setLabel] = useState("");
  // Anyone who can open this page can add a passkey, an agent included, so
  // the snippet names only passkeys this browser's authenticator proved it
  // holds, with a touch.
  // Each confirmed passkey's fingerprint as computed here from the key its
  // touch verified with, never the daemon's stored text.
  const [held, setHeld] = useState<ReadonlyMap<string, string>>(new Map());
  const [confirming, setConfirming] = useState<Failure | "busy" | null>(null);
  const pinned = passkeys.flatMap((passkey) => {
    const fingerprint = held.get(passkey.id);
    return fingerprint === undefined ? [] : [{ ...passkey, fingerprint }];
  });
  const snippet = policySnippet({ identity, passkeys: pinned }, site);
  async function confirm() {
    setConfirming("busy");
    try {
      const challenge = crypto.getRandomValues(new Uint8Array(32));
      const asserted = await assert({
        challenge,
        rpId: passkeys[0]?.rpId ?? "reef.localhost",
        credentialIds: passkeys.map((passkey) => passkey.credentialId),
      });
      const used = asserted.credentialId ?? new Uint8Array();
      const found = passkeys.find((passkey) => equal(passkey.credentialId, used));
      // The touch proves the person holds the passkey only if its signature
      // verifies with the key the daemon holds for it: a registration that
      // reuses the person's credential id with another key fails here.
      const held =
        found &&
        (await assertionVerifies(
          found,
          {
            authenticatorData: asserted.authenticatorData ?? new Uint8Array(),
            clientDataJson: asserted.clientDataJson ?? new Uint8Array(),
            signature: asserted.signature ?? new Uint8Array(),
          },
          challenge,
          window.location.origin,
          found.rpId,
        ));
      if (!found || !held) {
        setConfirming({
          kind: "browser",
          words:
            "That passkey's touch does not match the key Reef holds for it. Do not pin it: remove it, or reset this identity's passkeys.",
        });
        return;
      }
      const fingerprint = hex(await sha256(found.publicKey));
      setHeld((ids) => new Map([...ids, [found.id, fingerprint]]));
      setConfirming(null);
    } catch (thrown) {
      setConfirming(failure(thrown));
    }
  }
  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    onAdd(label.trim());
  }
  return (
    <section className="card passkey-person" aria-labelledby={`passkeys-${identity.id}`}>
      <h2 id={`passkeys-${identity.id}`}>
        {identity.name} {identity.isDefault && <span className="label">default</span>}
      </h2>
      {passkeys.length === 0 ? (
        <p className="secondary">
          No passkeys. Add one to sign off as {identity.name}; its first passkey needs no other.
        </p>
      ) : (
        <>
          <div className="table-box">
            <table>
              <thead>
                <tr>
                  <th>Label</th>
                  <th>Fingerprint</th>
                  <th>Added</th>
                  <th>Last used</th>
                  <th>
                    <span className="visually-hidden">Remove</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {passkeys.map((passkey) => (
                  <tr key={passkey.id}>
                    <td>{passkey.label}</td>
                    <td className="mono fingerprint">{passkey.fingerprint}</td>
                    <td>
                      <When at={passkey.createdAt} />
                    </td>
                    <td>{passkey.lastUsedAt ? <When at={passkey.lastUsedAt} /> : "never"}</td>
                    <td>
                      <button
                        type="button"
                        disabled={busy || paired}
                        aria-label={`Remove ${passkey.label}`}
                        onClick={() => {
                          onRemove(passkey);
                        }}
                      >
                        Remove
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="secondary">
            A repository&apos;s policy counts these passkeys only once its{" "}
            <code>reef-policy.toml</code> pins them. Pin only a passkey you hold: confirm it with a
            touch, and the snippet names it.
          </p>
          <p>
            <button
              type="button"
              disabled={busy || paired || confirming === "busy"}
              onClick={() => void confirm()}
            >
              Confirm a passkey to pin
            </button>
          </p>
          {confirming !== null && confirming !== "busy" && <FailureView failure={confirming} />}
          {pinned.length > 0 && (
            <>
              <p className="secondary">
                Confirmed by a touch:{" "}
                {pinned
                  .map((passkey) => `“${passkey.label}” (${passkey.fingerprint.slice(0, 12)})`)
                  .join(", ")}
                . Pin only a passkey you recognize.
              </p>
              {site ? (
                <>
                  <pre className="snippet">{snippet}</pre>
                  <p>
                    <Copy text={snippet} label="Copy policy snippet" />
                  </p>
                </>
              ) : (
                <p>
                  A pin names this page&apos;s address, and this daemon&apos;s port changes at every
                  start. Set <code>[ui] listen = &quot;127.0.0.1:&lt;port&gt;&quot;</code> in its{" "}
                  <code>config.toml</code>, restart it, and confirm again for the line to pin.
                </p>
              )}
            </>
          )}
        </>
      )}
      <form className="passkey-add" onSubmit={submit}>
        <label>
          Label{" "}
          <input
            value={label}
            maxLength={64}
            required
            placeholder="MacBook Touch ID"
            onInput={(event) => {
              setLabel(event.currentTarget.value);
            }}
          />
        </label>
        <button type="submit" className="primary" disabled={busy || paired || !label.trim()}>
          Add a passkey
        </button>
      </form>
      {passkeys.length > 0 && (
        <p className="secondary">Adding or removing a passkey needs a touch of one already here.</p>
      )}
      {action?.kind === "busy" && <p className="secondary">{action.words}</p>}
      {action?.kind === "done" && <p role="status">{action.words}</p>}
      {action?.kind === "failed" && <FailureView failure={action.failure} />}
    </section>
  );
}

export function Passkeys({
  connection,
  onSessionInvalid,
}: {
  connection: Connection;
  onSessionInvalid: () => void;
}) {
  const [version, setVersion] = useState(0);
  const load = usePeople(connection, version, onSessionInvalid);
  const [action, setAction] = useState<Action | null>(null);
  const busy = action?.kind === "busy";

  async function run(identity: string, steps: (say: (words: string) => void) => Promise<string>) {
    const say = (words: string) => {
      setAction({ kind: "busy", identity, words });
    };
    try {
      const done = await steps(say);
      setAction({ kind: "done", identity, words: done });
    } catch (thrown) {
      const failed = failure(thrown);
      if (failed.kind === "daemon" && isSessionInvalid(failed.error)) onSessionInvalid();
      else setAction({ kind: "failed", identity, failure: failed });
    } finally {
      setVersion((v) => v + 1);
    }
  }

  function add({ identity, passkeys }: Person, label: string) {
    void run(identity.id, async (say) => {
      const identities = client(connection, IdentityService);
      say("Starting the registration.");
      const start = await identities.startPasskeyRegistration({ identity: identity.id });
      let approval;
      if (start.needsApproval) {
        say(`Touch one of ${identity.name}'s passkeys to approve adding another.`);
        approval = await assert({
          challenge: start.addChallenge,
          rpId: start.rpId,
          credentialIds: passkeys.map((passkey) => passkey.credentialId),
        });
      }
      say("Create the new passkey on your authenticator.");
      const made = await create(start);
      const { passkey } = await identities.registerPasskey({
        challenge: start.challenge,
        label,
        ...made,
        ...(approval ? { approval } : {}),
      });
      return `Added “${passkey?.label ?? label}”. Pin it in a repository's policy for its sign-offs to count there.`;
    });
  }

  function remove({ identity, passkeys }: Person, passkey: Passkey) {
    void run(identity.id, async (say) => {
      say(`Touch one of ${identity.name}'s passkeys to approve removing “${passkey.label}”.`);
      const assertion = await assert({
        challenge: await removalChallenge(passkey.id),
        rpId: passkey.rpId,
        credentialIds: passkeys.map((live) => live.credentialId),
      });
      await client(connection, IdentityService).removePasskey({ id: passkey.id, assertion });
      return `Removed “${passkey.label}”. Remove its fingerprint from any policy that pins it.`;
    });
  }

  return (
    <section className="passkeys">
      <h1>Passkeys</h1>
      <p className="secondary">
        A passkey lets a person, not an agent running as them, sign off on a change: each sign-off
        takes one touch that approves exactly that sign-off.
      </p>
      {!hasPasskeySupport() &&
        (embedded() || device() ? (
          <NoPasskeysHere />
        ) : (
          <FailureView failure={{ kind: "browser", words: NO_SUPPORT }} />
        ))}
      {load.kind === "loading" && <p className="secondary">Reading the identities.</p>}
      {load.kind === "failed" && <ErrorView error={load.error} />}
      {load.kind === "loaded" && load.people.length === 0 && (
        <p className="secondary">This daemon has no person identities.</p>
      )}
      {load.kind === "loaded" &&
        load.people.map((person) => (
          <PersonCard
            key={person.identity.id}
            person={person}
            site={load.site}
            action={action?.identity === person.identity.id ? action : null}
            busy={busy}
            onAdd={(label) => {
              add(person, label);
            }}
            onRemove={(passkey) => {
              remove(person, passkey);
            }}
          />
        ))}
    </section>
  );
}
