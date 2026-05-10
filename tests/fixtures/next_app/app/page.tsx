export default function HomePage() {
  async function saveHome() {
    "use server";
    return { ok: true };
  }

  const saveDraft = async () => {
    "use server";
    return { ok: true };
  };

  const plainArrow = async () => {
    return { ok: false };
  };

  return <main>home</main>;
}
