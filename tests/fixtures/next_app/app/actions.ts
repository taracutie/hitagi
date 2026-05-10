"use server";

export async function createPost(formData: FormData) {
  return { ok: true, title: formData.get("title") };
}

export async function deletePost(id: string) {
  return { ok: true, id };
}

async function archivePost(id: string) {
  return { ok: true, id };
}

export { archivePost };
