// The triage inbox (R3.33, ADR 0088): the home page across every repository,
// and each repository's Inbox tab. Three buckets in the design's order: what
// needs you now, what you are watching, and what landed since you last
// looked, as a digest. Rows never reorder under the cursor: new items wait
// behind a count in their bucket's header, and an item that left keeps its
// row, dimmed, until the person shows the new ones. A help request is
// answered here; everything else opens the page that acts on it. All text an
// agent wrote is rendered as text (S-262).
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import type { Connection } from "../api/connection";
import { reefError } from "../api/errors";
import { device, DEVICE_READS } from "../device";
import type { InboxItem, LandedDigest } from "../gen/reef/api/v1/inbox_pb";
import { InboxReason } from "../gen/reef/api/v1/inbox_pb";
import { departedCount, type HeldBucket, type HeldRow } from "../inbox/held";
import { inboxTitle } from "../inbox/needs";
import { after } from "../live/backoff";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { ErrorView } from "../ui/ErrorView";
import { useNarrow } from "../ui/narrow";
import { Link } from "../ui/router";
import { useElementAnchor } from "../ui/scroll-anchor";
import { Time } from "../ui/TimeLabel";
import { plural } from "../ui/words";
import { activityHref } from "./activity-route";
import { Answer } from "./AnswerBox";
import { InboxModel, type BucketName, type DigestState } from "./inbox-model";
import { targetOf } from "./inbox-route";
import { changeLabel } from "./proposal-words";
import { smartlogHref } from "./smartlog-route";

/** How long a digest stays on screen in a visible tab before it counts as seen. */
export const SEEN_AFTER_MS = 3000;

const REASON_WORDS: Record<InboxReason, string> = {
  [InboxReason.UNSPECIFIED]: "",
  [InboxReason.HELP]: "help",
  [InboxReason.SIGNOFF]: "sign-off",
  [InboxReason.CONFLICT_UNOWNED]: "conflict",
  [InboxReason.CONFLICT_BLOCKED]: "conflict",
  [InboxReason.RETURNED]: "returned",
  [InboxReason.NO_EXECUTOR]: "executor",
  [InboxReason.ATTENTION]: "workspace",
  [InboxReason.CONFIRM]: "confirm",
  [InboxReason.IN_REVIEW]: "proposal",
  [InboxReason.IN_FLIGHT]: "instruction",
};

const DEPARTED_WORDS = {
  answered: "Answered",
  moved: "Moved to the other bucket",
  done: "No longer waiting",
} as const;

/**
 * A row's words: its link (a proposal's subject, else the daemon's
 * sentence), the sentence under it for a proposal, and the agent's own
 * words for a comment (a help request's question, else its text).
 */
function wordsOf(item: InboxItem): { title: string; status: string; text: string; tried: string } {
  const record = item.record;
  if (record.case === "comment") {
    const help = record.value.help;
    return {
      title: item.headline,
      status: "",
      text: help?.question || record.value.text,
      tried: help?.tried ?? "",
    };
  }
  if (record.case === "proposal") {
    const subject = record.value.change?.description.split("\n")[0]?.trim() ?? "";
    return { title: subject || "(no description)", status: item.headline, text: "", tried: "" };
  }
  return { title: item.headline, status: "", text: "", tried: "" };
}

function Row({
  row,
  repoName,
  opened,
  connection,
  model,
  register,
}: {
  row: HeldRow;
  repoName: string | undefined;
  /** The item the page was opened at: its answer is open, and it is in view. */
  opened: boolean;
  connection: Connection;
  model: InboxModel;
  register: (key: string, element: HTMLElement | null) => void;
}) {
  const { item, departed } = row;
  const paired = device() !== null;
  const [answering, setAnswering] = useState(opened);
  const element = useRef<HTMLLIElement | null>(null);
  useEffect(() => {
    if (opened) element.current?.scrollIntoView({ block: "start" });
  }, [opened]);
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState("");
  const { title, status, text, tried } = wordsOf(item);
  const help =
    item.reason === InboxReason.HELP && item.record.case === "comment" ? item.record.value : null;
  return (
    <li
      ref={(now) => {
        element.current = now;
        register(row.key, now);
      }}
      className={`inbox-row${departed ? " departed" : ""}${opened ? " opened" : ""}`}
      data-key={row.key}
    >
      <span className="inbox-reason">{REASON_WORDS[item.reason]}</span>
      <div className="inbox-row-text">
        <Link to={targetOf(item)} className="name-link">
          {title}
        </Link>
        {status && <span className="inbox-status">{status}</span>}
        {text && <span className="pre-wrap">{text}</span>}
        {tried && <span className="secondary pre-wrap">Tried: {tried}</span>}
        <span className="secondary">
          {repoName && `${repoName} · `}
          {item.changeId && <span className="mono">{changeLabel(item.changeId)} · </span>}
          <Time at={item.since} />
          {departed && ` · ${DEPARTED_WORDS[departed]}`}
        </span>
        {item.next.length > 0 && !departed && (
          <span className="secondary">
            Run <code>{item.next.join(" ")}</code>
          </span>
        )}
        {help && !departed && paired && <span className="secondary">{DEVICE_READS}</span>}
        {help && !departed && !paired && !answering && (
          <span>
            <button
              type="button"
              onClick={() => {
                setAnswering(true);
              }}
            >
              Answer
            </button>
          </span>
        )}
        {help && answering && !paired && !departed && (
          <Answer
            repo={item.repoId}
            change={item.changeId}
            commit={item.commitId}
            help={help}
            connection={connection}
            busy={busy}
            run={(write) => {
              setBusy(true);
              write().then(
                (words) => {
                  setBusy(false);
                  setSaid(words);
                  model.answeredItem(row.key);
                },
                (thrown: unknown) => {
                  setBusy(false);
                  setSaid(reefError(thrown).message);
                },
              );
            }}
          />
        )}
        {said && (
          <span className="secondary" role="status">
            {said}
          </span>
        )}
      </div>
    </li>
  );
}

/** A bucket with its heading; `folded`, the heading opens it. */
function Fold({
  title,
  count,
  folded,
  open = false,
  children,
}: {
  title: string;
  count: number | undefined;
  folded: boolean;
  /** It holds the item the page was opened at. */
  open?: boolean;
  children: ReactNode;
}) {
  const heading = (
    <h2>
      {title} {count !== undefined && <span className="secondary">({count})</span>}
    </h2>
  );
  return (
    <section className="inbox-bucket" aria-label={title}>
      {folded ? (
        <details open={open}>
          <summary>{heading}</summary>
          {children}
        </details>
      ) : (
        <>
          {heading}
          {children}
        </>
      )}
    </section>
  );
}

function Bucket({
  name,
  title,
  held,
  empty,
  repoNames,
  item,
  folded,
  connection,
  model,
  version,
  reading,
}: {
  name: BucketName;
  title: string;
  held: HeldBucket | null;
  reading: boolean;
  empty: string;
  /** The key of the item the page was opened at; empty for none. */
  item: string;
  /** Whether the bucket is folded until opened, as on a phone. */
  folded: boolean;
  repoNames: ReadonlyMap<string, string> | undefined;
  connection: Connection;
  model: InboxModel;
  version: number;
}) {
  const keys = held?.rows.map((row) => row.key) ?? [];
  const register = useElementAnchor(keys, version, "");
  const fresh = held?.fresh.length ?? 0;
  const departed = held ? departedCount(held) : 0;
  const shownTotal = held?.rows.length ?? 0;
  return (
    <Fold
      title={title}
      count={held?.total}
      folded={folded}
      open={held?.rows.some((row) => row.key === item) ?? false}
    >
      {(fresh > 0 || departed > 0) && (
        <p className="notice" role="status">
          {fresh > 0 && `${plural(fresh, "new item")}. `}
          <button
            type="button"
            onClick={() => {
              model.show(name);
            }}
          >
            {fresh > 0 ? `Show ${plural(fresh, "new item")}` : "Clear those done"}
          </button>
        </p>
      )}
      {held === null && <p className="secondary">Reading the inbox.</p>}
      {held && held.rows.length === 0 && fresh === 0 && !reading && (
        <p className="secondary">{empty}</p>
      )}
      {held && reading && <p className="secondary">Reading more repositories.</p>}
      {held && held.rows.length > 0 && (
        <ul className="card inbox-list">
          {held.rows.map((row) => (
            <Row
              key={row.key}
              row={row}
              repoName={repoNames?.get(row.item.repoId)}
              opened={row.key === item}
              connection={connection}
              model={model}
              register={register}
            />
          ))}
        </ul>
      )}
      {held && held.total > shownTotal + fresh && (
        <p className="secondary">
          Showing the newest {shownTotal + fresh} of {held.total}; run <code>reef inbox</code> for
          the rest.
        </p>
      )}
    </Fold>
  );
}

/** True while `element` is on screen in a visible tab. */
function useSeen(element: HTMLElement | null): boolean {
  const [inView, setInView] = useState(() => typeof IntersectionObserver === "undefined");
  const [visible, setVisible] = useState(
    () => typeof document === "undefined" || document.visibilityState !== "hidden",
  );
  useEffect(() => {
    const onVisibility = () => {
      setVisible(document.visibilityState !== "hidden");
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);
  useEffect(() => {
    if (!element || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => {
      setInView(entries.some((entry) => entry.isIntersecting));
    });
    observer.observe(element);
    return () => {
      observer.disconnect();
    };
  }, [element]);
  return inView && visible;
}

function Digest({
  state,
  repoName,
  model,
}: {
  state: DigestState;
  repoName: string | undefined;
  model: InboxModel;
}) {
  const [element, setElement] = useState<HTMLElement | null>(null);
  const seen = useSeen(element);
  const digest: LandedDigest = state.digest;
  const through = digest.throughSeq;
  const unmarked = through > state.marked;
  useEffect(() => {
    if (!seen || !unmarked) return;
    return after(SEEN_AFTER_MS, () => {
      void model.markSeen(state.repoId, through);
    });
  }, [seen, unmarked, model, state.repoId, through]);
  const count = Number(digest.count);
  return (
    <li
      ref={setElement}
      className="inbox-digest"
      data-repo={state.repoId}
      data-seen={unmarked ? "no" : "yes"}
    >
      <p>
        <strong>{plural(count, "change")} landed</strong>
        {repoName && ` in ${repoName}`}{" "}
        <span className="secondary">
          {digest.fromMark ? "since you last looked, " : "in the last day, "}
          <Time at={digest.since} />
        </span>
      </p>
      {digest.byAgent.length > 0 && (
        <p className="secondary">
          {digest.byAgent
            .map((agent) =>
              agent.agent
                ? `${String(agent.count)} by ${agent.agent}`
                : `${String(agent.count)} with no agent named`,
            )
            .join(" · ")}
        </p>
      )}
      <ul className="inbox-landings">
        {digest.newest.map((landing) => (
          <li key={landing.changeId}>
            <Link to={smartlogHref({ repo: state.repoId, change: landing.changeId })}>
              <span className="mono">{changeLabel(landing.changeId)}</span>
            </Link>{" "}
            {landing.subject || "(no description)"}{" "}
            <span className="secondary">
              {landing.agent && `${landing.agent} · `}
              <Time at={landing.landedAt} />
            </span>
          </li>
        ))}
      </ul>
      {count > digest.newest.length && (
        <p className="secondary">
          <Link to={activityHref({ repo: state.repoId })}>
            And {plural(count - digest.newest.length, "more")} in the activity feed
          </Link>
        </p>
      )}
    </li>
  );
}

export function Inbox({
  repo,
  at,
  item = "",
  repoNames,
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  /** The repository the tab names; undefined on the home page. */
  repo?: string | undefined;
  at?: string;
  /** The key of the item the page was opened at. */
  item?: string;
  /** Repository names by id, for rows across repositories. */
  repoNames?: ReadonlyMap<string, string>;
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const [model] = useState(
    () => new InboxModel(connection, live.inbox, frame, onSessionInvalid, repo),
  );
  useEffect(() => model.start(), [model]);
  const snapshot = useSyncExternalStore(model.subscribe, model.getSnapshot);
  const names = repo ? undefined : repoNames;
  const narrow = useNarrow();
  // Before paint, so the count in the tab's title never lags the page.
  useLayoutEffect(() => {
    document.title = inboxTitle(snapshot.needsTotal);
  }, [snapshot.needsTotal]);
  return (
    <div className="inbox">
      {!repo && <h1>Inbox</h1>}
      {at && <p className="notice">The inbox is read live, not at an operation.</p>}
      {snapshot.error && <ErrorView error={snapshot.error} />}
      {snapshot.failed.map((section) => (
        <p className="notice" key={section.repoId}>
          Could not read {names?.get(section.repoId) ?? section.repoId}: {section.error?.message}
        </p>
      ))}
      <Bucket
        name="needs"
        title="Needs you now"
        held={snapshot.needs}
        reading={snapshot.reading}
        empty="Nothing needs you now."
        repoNames={names}
        item={item}
        folded={false}
        connection={connection}
        model={model}
        version={snapshot.version}
      />
      <Bucket
        name="watching"
        title="Watching"
        held={snapshot.watching}
        reading={snapshot.reading}
        empty="No proposals or instructions in flight."
        repoNames={names}
        item={item}
        folded={narrow}
        connection={connection}
        model={model}
        version={snapshot.version}
      />
      <Fold title="Landed since you last looked" count={undefined} folded={narrow}>
        {snapshot.needs !== null && snapshot.digests.length === 0 && (
          <p className="secondary">Nothing new has landed.</p>
        )}
        {snapshot.digests.length > 0 && (
          <ul className="card inbox-list">
            {snapshot.digests.map((state) => (
              <Digest
                key={state.repoId}
                state={state}
                repoName={names?.get(state.repoId)}
                model={model}
              />
            ))}
          </ul>
        )}
      </Fold>
    </div>
  );
}
