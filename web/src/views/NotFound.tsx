import { Link } from "../ui/router";

export function NotFound() {
  return (
    <div className="card center-card">
      <h1>Page not found</h1>
      <p className="prose">
        Nothing is here. <Link to="/">Back to the overview</Link>.
      </p>
    </div>
  );
}
