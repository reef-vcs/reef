interface Props {
  label: string;
}

export function Button(props: Props) {
  return <button onClick={() => track(props.label)}>{props.label}</button>;
}

export const Panel = () => <div>{Button({ label: "ok" })}</div>;
