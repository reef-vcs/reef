// An in-memory daemon for the passkey and sign-off tests (Connect's router
// transport): identities, passkeys, approvals and evidence, the requests the
// pages make, and a refusal per method when a test sets one. Approvals carry
// the challenge the real daemon derives, and wait as `GetApproval` does.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { durationMs, timestampFromMs, timestampNow } from "@bufbuild/protobuf/wkt";
import { createRouterTransport, type ConnectRouter } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import {
  EvaluatePolicyResponseSchema,
  PolicyService,
  PolicyVerifiedSignerSchema,
} from "../gen/reef/api/v1/policy_pb";
import {
  ChangeSchema,
  ChangeService,
  GetChangeResponseSchema,
  ListChangesResponseSchema,
} from "../gen/reef/api/v1/changes_pb";
import {
  AddEvidenceResponseSchema,
  EvidenceResult,
  EvidenceSchema,
  EvidenceService,
  ListEvidenceResponseSchema,
  type AddEvidenceRequest,
  type Evidence,
} from "../gen/reef/api/v1/evidence_pb";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import {
  ApprovalSchema,
  ApprovalState,
  AuthoritySchema,
  IdentityKind,
  IdentitySchema,
  IdentityService,
  IdentityStatus,
  KeyIdentitySchema,
  PasskeyRefSchema,
  PasskeySchema,
  SessionKeySchema,
  SessionPresence,
  type Approval,
  type Identity,
  type Passkey,
  type RegisterPasskeyRequest,
  type RemovePasskeyRequest,
  type RequestApprovalRequest,
  type ResolveApprovalRequest,
} from "../gen/reef/api/v1/identity_pb";
import { hex, sha256 } from "../sign/bytes";
import { approvalChallenge } from "../sign/key";
import { CREDENTIAL_ID } from "./webauthn";

export const AUTHORITY = "9064b45b3e0fb33b851a2e994e8bbf6f60ef5c1579301e88719cc13ae3153aec";
export const TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
export const COMMIT = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";
export const CHANGE = "8d1f0e7c6b5a49382716f5e4d3c2b1a0";
export const REPO = "a3dda866c7e8f343";
const PASSKEY_REF = create(PasskeyRefSchema, {
  id: "pk_1",
  label: "MacBook Touch ID",
  fingerprint: "ab".repeat(32),
});

export function person(
  name: string,
  init: Exclude<MessageInitShape<typeof IdentitySchema>, Identity> = {},
): Identity {
  return create(IdentitySchema, {
    id: `idn_${name.padEnd(16, "0").slice(0, 16)}`,
    name,
    kind: IdentityKind.PERSON,
    ...init,
  });
}

export function passkey(identity: Identity, label: string, n: number): Passkey {
  return create(PasskeySchema, {
    id: `pk_${String(n).padStart(16, "0")}`,
    identityId: identity.id,
    identity: identity.name,
    label,
    fingerprint: String(n).repeat(64).slice(0, 64),
    credentialId: CREDENTIAL_ID,
    rpId: "localhost",
    createdAt: timestampFromMs(1_795_000_000_000),
  });
}

type Method =
  | "requestApproval"
  | "resolveApproval"
  | "addEvidence"
  | "registerPasskey"
  | "removePasskey"
  | "getApproval";

export class IdentityDaemon {
  /** Whether `[ui] listen` names a port. */
  fixedPort = true;
  readonly arsood = person("arsood", { isDefault: true });
  identities: Identity[] = [this.arsood, person("ci-bot", { kind: IdentityKind.CI })];
  passkeys: Passkey[] = [];
  readonly approvals = new Map<string, Approval>();
  evidence: Evidence[] = [];
  /** The descriptions of the changes below the approval's that have not landed. */
  below: string[] = [];
  /** What `EvaluatePolicy` reports of the `user_verified` signers; none without a policy. */
  verified: MessageInitShape<typeof PolicyVerifiedSignerSchema>[] = [];
  readonly fail: Partial<Record<Method, Error>> = {};
  readonly requested: RequestApprovalRequest[] = [];
  readonly resolved: ResolveApprovalRequest[] = [];
  readonly added: AddEvidenceRequest[] = [];
  readonly registered: RegisterPasskeyRequest[] = [];
  readonly removed: RemovePasskeyRequest[] = [];
  private waiters: (() => void)[] = [];

  /** A pending approval as the CLI's request makes one. */
  async pending(
    init: Exclude<MessageInitShape<typeof ApprovalSchema>, Approval> = {},
  ): Promise<Approval> {
    const statement = new TextEncoder().encode('{"_type":"https://in-toto.io/Statement/v1"}');
    const approval = create(ApprovalSchema, {
      id: "apr_00000000000000a1",
      state: ApprovalState.PENDING,
      identityId: this.arsood.id,
      identity: this.arsood.name,
      keyId: "d4".repeat(32),
      sessionId: "ses_41c9e07ab2d35f18",
      authorityKeyId: AUTHORITY,
      approves: { statementSha256: hex(await sha256(statement)), changeId: CHANGE },
      rpId: "localhost",
      credentialIds: this.passkeys.map((p) => p.credentialId),
      kind: "signoff",
      result: "pass",
      treeId: TREE,
      repoId: REPO,
      commitId: COMMIT,
      changeId: CHANGE,
      statement,
      requestedThrough: "socket",
      requestedByAgent: "claude@mac",
      requestedWorkspace: "6ce393d29a64e06c",
      createdAt: timestampNow(),
      expiresAt: timestampFromMs(Date.now() + 120_000),
      ...init,
    });
    approval.challenge = await this.challenge(approval);
    this.approvals.set(approval.id, approval);
    return approval;
  }

  private async challenge(approval: Approval): Promise<Uint8Array> {
    return approvalChallenge({
      authority: approval.authorityKeyId,
      key: approval.keyId,
      identityId: approval.identityId,
      session: approval.sessionId,
      statement: approval.approves?.statementSha256 ?? "",
      change: approval.changeId,
    });
  }

  private settle(approval: Approval, state: ApprovalState) {
    approval.state = state;
    approval.resolvedAt = timestampNow();
    for (const wake of this.waiters.splice(0)) wake();
  }

  expire(id: string) {
    const approval = this.approvals.get(id);
    if (approval) this.settle(approval, ApprovalState.EXPIRED);
  }

  connection(): Connection {
    const transport = createRouterTransport((router) => {
      this.routes(router);
    });
    return { transport, label: "test" };
  }

  /** The services, for a fake daemon that adds its own in place of `changes` and `policy`. */
  routes(
    router: ConnectRouter,
    { changes = true, policy = true }: { changes?: boolean; policy?: boolean } = {},
  ) {
    const service = router.service.bind(router);
    service(DaemonService, {
      getDaemonStatus: () => ({ webUi: { fixed: this.fixedPort } }),
    });
    service(IdentityService, {
      getIdentity: () => ({ identity: this.identities.find((i) => i.isDefault) }),
      listIdentities: () => ({ identities: this.identities }),
      listPasskeys: (request) => ({
        passkeys: this.passkeys.filter(
          (p) => !request.identity || p.identityId === request.identity,
        ),
      }),
      listAuthorities: () => ({
        authorities: [create(AuthoritySchema, { keyId: AUTHORITY, current: true })],
      }),
      startPasskeyRegistration: (request) => {
        const identity = this.identities.find((i) => i.id === request.identity) ?? this.arsood;
        const live = this.passkeys.filter((p) => p.identityId === identity.id);
        return {
          challenge: new Uint8Array(32).fill(0x0c),
          rpId: "localhost",
          identityId: identity.id,
          identity: identity.name,
          userName: `Reef: ${identity.name}`,
          algorithms: [-8, -7, -257],
          credentialIds: live.map((p) => p.credentialId),
          needsApproval: live.length > 0,
          addChallenge: live.length > 0 ? new Uint8Array(32).fill(0xad) : new Uint8Array(),
        };
      },
      registerPasskey: (request) => {
        if (this.fail.registerPasskey) throw this.fail.registerPasskey;
        this.registered.push(request);
        const added = passkey(this.arsood, request.label, this.passkeys.length + 1);
        this.passkeys.push(added);
        return { passkey: added };
      },
      removePasskey: (request) => {
        if (this.fail.removePasskey) throw this.fail.removePasskey;
        this.removed.push(request);
        const gone = this.passkeys.find((p) => p.id === request.id);
        this.passkeys = this.passkeys.filter((p) => p.id !== request.id);
        return { passkey: gone };
      },
      requestApproval: async (request) => {
        if (this.fail.requestApproval) throw this.fail.requestApproval;
        this.requested.push(request);
        const approval = await this.pending({
          id: `apr_${String(this.requested.length).padStart(16, "0")}`,
          keyId: hex(await sha256(request.publicKey)),
          approves: {
            statementSha256: hex(await sha256(request.statement)),
            changeId: CHANGE,
          },
          statement: request.statement,
          commitId: request.revision,
          requestedThrough: "web",
          requestedByAgent: "",
          requestedWorkspace: "",
        });
        return { approval };
      },
      getApproval: async (request, context) => {
        if (this.fail.getApproval) throw this.fail.getApproval;
        const approval = this.approvals.get(request.id);
        if (!approval) throw new Error("no approval");
        if (
          request.wait &&
          durationMs(request.wait) > 0 &&
          approval.state === ApprovalState.PENDING
        ) {
          await new Promise<void>((resolve, reject) => {
            this.waiters.push(resolve);
            context.signal.addEventListener("abort", () => {
              reject(new Error("aborted"));
            });
          });
        }
        return { approval };
      },
      resolveApproval: (request) => {
        if (this.fail.resolveApproval) throw this.fail.resolveApproval;
        this.resolved.push(request);
        const approval = this.approvals.get(request.id);
        if (!approval) throw new Error("no approval");
        if (request.resolution.case === "approve") {
          approval.passkey = PASSKEY_REF;
          this.settle(approval, ApprovalState.APPROVED);
        } else {
          approval.declinedReason = request.resolution.value ?? "";
          this.settle(approval, ApprovalState.DECLINED);
        }
        return { approval };
      },
    });
    service(EvidenceService, {
      listEvidence: () => create(ListEvidenceResponseSchema, { evidence: this.evidence }),
      addEvidence: (request) => {
        if (this.fail.addEvidence) throw this.fail.addEvidence;
        this.added.push(request);
        const evidence = create(EvidenceSchema, {
          evidenceId: "e".repeat(64),
          kind: "signoff",
          result: EvidenceResult.PASS,
          current: true,
          addedAt: timestampNow(),
          identity: create(KeyIdentitySchema, {
            status: IdentityStatus.BOUND,
            key: create(SessionKeySchema, {
              identity: "arsood",
              presence: SessionPresence.USER_VERIFIED,
              passkey: PASSKEY_REF,
            }),
          }),
        });
        this.evidence = [evidence, ...this.evidence];
        return create(AddEvidenceResponseSchema, { evidence, created: true });
      },
    });
    if (policy) {
      service(PolicyService, {
        evaluatePolicy: () =>
          create(EvaluatePolicyResponseSchema, { evaluation: { verified: this.verified } }),
      });
    }
    if (!changes) return;
    service(ChangeService, {
      listChanges: () =>
        create(ListChangesResponseSchema, {
          entries: this.below.map((description, n) => ({
            change: {
              changeId: String(n).repeat(32),
              commitId: String(n).repeat(40),
              description,
            },
          })),
        }),
      getChange: () =>
        create(GetChangeResponseSchema, {
          change: create(ChangeSchema, {
            changeId: CHANGE,
            commitId: COMMIT,
            treeId: TREE,
            description: "Fix the lease renewal race\n\nLonger body.",
            author: { name: "Ada Agent", email: "ada@example.com" },
          }),
        }),
    });
  }
}
