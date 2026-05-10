import type { GetServerSidePropsContext } from "next";

export async function getServerSideProps(ctx: GetServerSidePropsContext) {
  return { props: { id: ctx.params?.id ?? "" } };
}

export default function UserPage({ id }: { id: string }) {
  return <section>user {id}</section>;
}
