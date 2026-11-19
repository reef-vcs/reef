// `/passkeys` (R3.43): each person identity's passkeys, the `[passkeys]`
// lines that pin them in a repository's policy, and the ceremonies that add
// and remove them. A passkey verifies a person for a sign-off; a policy
// counts it only where it pins the fingerprint.
import { useEffect, useState, type SubmitEvent } from "react";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import {
  IdentityKind,
  IdentityService,
  type Identity,
  type Passkey,
} from "../gen/reef/api/v1/identity_pb";
import { removalChallenge } from "../sign/key";
import {
  assert,
  create,
  failure,
  hasPasskeySupport,
  NO_SUPPORT,
  type Failure,
} from "../sign/passkey";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { FailureView } from "../ui/FailureView";
import { dateTime } from "../ui/time";

const PAGE = 1_000;

interface Person {
  identity: Identity;
  passkeys: Passkey[];
}

type Load =
  { kind: "loading" } | { kind: "failed"; error: ReefError } | { kind: "loaded"; people: Person[] };

/** What one identity's last add or remove is doing, or how it ended. */
type Action =
  | { kind: "busy"; identity: string; words: string }
  | { kind: "done"; identity: string; words: string }
  | { kind: "failed"; identity: string; failure: Failure };

const TOML_BARE_KEY = /^[A-Za-z0-9_-]+$/;

/** The `[passkeys]` table pinning every live passkey of `person`. */
export function policySnippet({ identity, passkeys }: Person): string {
  const key = TOML_BARE_KEY.test(identity.name) ? identity.name : JSON.stringify(identity.name);
  const fingerprints = passkeys.map((passkey) => JSON.stringify(passkey.fingerprint)).join(", ");
  return `[passkeys]\n${key} = [${fingerprints}]\n`;
}

function usePeople(connection: Connection, version: number, onSessionInvalid: () => void) {
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  useEffect(() => {
    let current = true;
    const identities = client(connection, IdentityService);
    Promise.all([
      identities.listIdentities({ pageSize: PAGE }),
      identities.listPasskeys({ pageSize: PAGE }),
    ]).then(
      ([listed, keys]) => {
        if (!current) return;
        const people = listed.identities
          .filter((identity) => identity.kind === IdentityKind.PERSON && !identity.revocation)
          .map((identity) => ({
            identity,
            passkeys: keys.passkeys.filter((passkey) => passkey.identityId === identity.id),
          }))
          .sort((a, b) => Number(b.identity.isDefault) - Number(a.identity.isDefault));
        setLoad({ kind: "loaded", people });
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
  action,
  busy,
  onAdd,
  onRemove,
}: {
  person: Person;
  action: Action | null;
  busy: boolean;
  onAdd: (label: string) => void;
  onRemove: (passkey: Passkey) => void;
}) {
  const { identity, passkeys } = person;
  const [label, setLabel] = useState("");
  const snippet = policySnippet(person);
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
                        disabled={busy}
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
            <code>reef-policy.toml</code> pins them:
          </p>
          <pre className="snippet">{snippet}</pre>
          <p>
            <Copy text={snippet} label="Copy policy snippet" />
          </p>
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
        <button type="submit" className="primary" disabled={busy || !label.trim()}>
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
      {!hasPasskeySupport() && <FailureView failure={{ kind: "browser", words: NO_SUPPORT }} />}
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
