import { Link } from "../ui/router";

export function NotFound() {
  return (
    <>
      <h2>Not found</h2>
      <p>
        Nothing is here. <Link to="/">Back to the overview</Link>.
      </p>
    </>
  );
}
