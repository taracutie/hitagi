export async function submitForm(formData: FormData) {
  "use server";
  return { ok: true, value: formData.get("value") };
}

export async function plainHelper(x: number) {
  return x * 2;
}
