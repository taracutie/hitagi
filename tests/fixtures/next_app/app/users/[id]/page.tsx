export default function UserPage({ params }: { params: { id: string } }) {
  return <section>user {params.id}</section>;
}
