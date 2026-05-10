"use client";

export default function RootError({ error }: { error: Error }) {
  return <div>error: {error.message}</div>;
}
