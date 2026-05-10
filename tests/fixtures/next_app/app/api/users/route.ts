export async function GET() {
  return new Response("[]");
}

export const POST = async () => {
  return new Response("ok", { status: 201 });
};

async function DELETE() {
  return new Response(null, { status: 204 });
}

const updateUser = async () => {
  return new Response("updated");
};

export { DELETE, updateUser as PATCH };
