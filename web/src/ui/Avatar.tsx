// Avatars and identities (the design pass). An avatar is an initial in a
// circle, colored by the name; decoration only, so the letter is drawn by
// CSS (`data-initial`) and never enters a text or an accessible name. The
// name is always beside it.
import { avatarIndex, initialOf, splitIdentity } from "./identity";

export type AvatarSize = "sm" | "md" | "lg";

export function Avatar({ name, size = "md" }: { name: string; size?: AvatarSize }) {
  return (
    <span
      className={`avatar avatar-${String(avatarIndex(name))} avatar-${size}`}
      data-initial={initialOf(name)}
      aria-hidden="true"
    />
  );
}

/** Up to `max` avatars, overlapping, each named in `title`, then "+n". */
export function AvatarStack({
  names,
  max = 3,
  size = "sm",
}: {
  names: string[];
  max?: number;
  size?: AvatarSize;
}) {
  if (names.length === 0) return null;
  const shown = names.slice(0, max);
  return (
    <span className="avatar-stack">
      {shown.map((name) => (
        <span key={name} title={name}>
          <Avatar name={name} size={size} />
        </span>
      ))}
      {names.length > max && <span className="more-count">+{names.length - max}</span>}
    </span>
  );
}

/**
 * The avatar, the name and a quieter `@host`, on one line with an ellipsis
 * and the whole identity in `title`. The text is still the full identity.
 */
export function Identity({ identity, size = "md" }: { identity: string; size?: AvatarSize }) {
  if (!identity) return <>-</>;
  const { name, host } = splitIdentity(identity);
  return (
    <span className="identity" title={identity}>
      <Avatar name={identity} size={size} />
      <span className="identity-text">
        <span className="identity-name">{name}</span>
        {host && <span className="identity-host">@{host}</span>}
      </span>
    </span>
  );
}
