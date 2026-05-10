export default function BlogPage({ params }: { params: { slug: string[] } }) {
  return <article>{params.slug.join("/")}</article>;
}
