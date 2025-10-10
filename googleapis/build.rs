use tonic_build;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tonic_build::configure()
    //     .build_server(false)
    //     .out_dir("./type/src")
    //     .compile(
    //         &["/root/googleapis/google/type/expr.proto"],
    //         &["/root/googleapis"],
    //     )?;
    // tonic_build::configure()
    //     .build_server(false)
    //     .out_dir("./iam/src")
    //     .extern_path(".google.type.Expr", "::google_type::Expr")
    //     .compile(
    //         &["/root/googleapis/google/iam/v1/policy.proto"],
    //         &["/root/googleapis"],
    //     )?;
    // tonic_build::configure()
    //     .build_server(false)
    //     .out_dir("./iam-policy/src")
    //     .extern_path(".google.type.Expr", "::google_type::Expr")
    //     .compile(
    //         &["/root/googleapis/google/iam/v1/iam_policy.proto"],
    //         &["/root/googleapis"],
    //     )?;
    // tonic_build::configure()
    //     .build_server(false)
    //     .out_dir("./storagev1/src")
    //     .extern_path(".google.type.Expr", "::google_type::Expr")
    //     .extern_path(".google.iam.v1.Policy", "::google_iam::Policy")
    //     .extern_path(
    //         ".google.iam.v1.TestIamPermissionsRequest",
    //         "::google_iam_iam_policy::TestIamPermissionsRequest",
    //     )
    //     .extern_path(
    //         ".google.iam.v1.TestIamPermissionsResponse",
    //         "::google_iam_iam_policy::TestIamPermissionsResponse",
    //     )
    //     .extern_path(
    //         ".google.iam.v1.GetIamPolicyRequest",
    //         "::google_iam_iam_policy::GetIamPolicyRequest",
    //     )
    //     .extern_path(
    //         ".google.iam.v1.SetIamPolicyRequest",
    //         "::google_iam_iam_policy::SetIamPolicyRequest",
    //     )
    //     .compile(
    //         &["/root/googleapis/google/storage/v1/storage.proto"],
    //         &["/root/googleapis"],
    //     )?;
    // tonic_build::configure()
    //     .build_server(false)
    //     .out_dir("./texttospeechv1/src")
    //     .compile(
    //         &["/root/googleapis/google/cloud/texttospeech/v1/cloud_tts.proto"],
    //         &["/root/googleapis"],
    //     )?;
    Ok(())
}
