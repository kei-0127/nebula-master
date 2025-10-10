/// Encapsulates settings provided to GetIamPolicy.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetPolicyOptions {
    /// Optional. The policy format version to be returned.
    ///
    /// Valid values are 0, 1, and 3. Requests specifying an invalid value will be
    /// rejected.
    ///
    /// Requests for policies with any conditional bindings must specify version 3.
    /// Policies without any conditional bindings may specify any valid value or
    /// leave the field unset.
    #[prost(int32, tag = "1")]
    pub requested_policy_version: i32,
}
/// Associates `members` with a `role`.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Binding {
    /// Role that is assigned to `members`.
    /// For example, `roles/viewer`, `roles/editor`, or `roles/owner`.
    #[prost(string, tag = "1")]
    pub role: ::prost::alloc::string::String,
    /// Specifies the identities requesting access for a Cloud Platform resource.
    /// `members` can have the following values:
    ///
    /// * `allUsers`: A special identifier that represents anyone who is
    ///    on the internet; with or without a Google account.
    ///
    /// * `allAuthenticatedUsers`: A special identifier that represents anyone
    ///    who is authenticated with a Google account or a service account.
    ///
    /// * `user:{emailid}`: An email address that represents a specific Google
    ///    account. For example, `alice@example.com` .
    ///
    ///
    /// * `serviceAccount:{emailid}`: An email address that represents a service
    ///    account. For example, `my-other-app@appspot.gserviceaccount.com`.
    ///
    /// * `group:{emailid}`: An email address that represents a Google group.
    ///    For example, `admins@example.com`.
    ///
    ///
    /// * `domain:{domain}`: The G Suite domain (primary) that represents all the
    ///    users of that domain. For example, `google.com` or `example.com`.
    ///
    ///
    #[prost(string, repeated, tag = "2")]
    pub members: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    /// The condition that is associated with this binding.
    /// NOTE: An unsatisfied condition will not allow user access via current
    /// binding. Different bindings, including their conditions, are examined
    /// independently.
    #[prost(message, optional, tag = "3")]
    pub condition: ::core::option::Option<::google_type::Expr>,
}
/// The difference delta between two policies.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PolicyDelta {
    /// The delta for Bindings between two policies.
    #[prost(message, repeated, tag = "1")]
    pub binding_deltas: ::prost::alloc::vec::Vec<BindingDelta>,
    /// The delta for AuditConfigs between two policies.
    #[prost(message, repeated, tag = "2")]
    pub audit_config_deltas: ::prost::alloc::vec::Vec<AuditConfigDelta>,
}
/// One delta entry for Binding. Each individual change (only one member in each
/// entry) to a binding will be a separate entry.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BindingDelta {
    /// The action that was performed on a Binding.
    /// Required
    #[prost(enumeration = "binding_delta::Action", tag = "1")]
    pub action: i32,
    /// Role that is assigned to `members`.
    /// For example, `roles/viewer`, `roles/editor`, or `roles/owner`.
    /// Required
    #[prost(string, tag = "2")]
    pub role: ::prost::alloc::string::String,
    /// A single identity requesting access for a Cloud Platform resource.
    /// Follows the same format of Binding.members.
    /// Required
    #[prost(string, tag = "3")]
    pub member: ::prost::alloc::string::String,
    /// The condition that is associated with this binding.
    #[prost(message, optional, tag = "4")]
    pub condition: ::core::option::Option<::google_type::Expr>,
}
/// Nested message and enum types in `BindingDelta`.
pub mod binding_delta {
    /// The type of action performed on a Binding in a policy.
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        ::prost::Enumeration,
    )]
    #[repr(i32)]
    pub enum Action {
        /// Unspecified.
        Unspecified = 0,
        /// Addition of a Binding.
        Add = 1,
        /// Removal of a Binding.
        Remove = 2,
    }
}
/// One delta entry for AuditConfig. Each individual change (only one
/// exempted_member in each entry) to a AuditConfig will be a separate entry.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AuditConfigDelta {
    /// The action that was performed on an audit configuration in a policy.
    /// Required
    #[prost(enumeration = "audit_config_delta::Action", tag = "1")]
    pub action: i32,
    /// Specifies a service that was configured for Cloud Audit Logging.
    /// For example, `storage.googleapis.com`, `cloudsql.googleapis.com`.
    /// `allServices` is a special value that covers all services.
    /// Required
    #[prost(string, tag = "2")]
    pub service: ::prost::alloc::string::String,
    /// A single identity that is exempted from "data access" audit
    /// logging for the `service` specified above.
    /// Follows the same format of Binding.members.
    #[prost(string, tag = "3")]
    pub exempted_member: ::prost::alloc::string::String,
    /// Specifies the log_type that was be enabled. ADMIN_ACTIVITY is always
    /// enabled, and cannot be configured.
    /// Required
    #[prost(string, tag = "4")]
    pub log_type: ::prost::alloc::string::String,
}
/// Nested message and enum types in `AuditConfigDelta`.
pub mod audit_config_delta {
    /// The type of action performed on an audit configuration in a policy.
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        ::prost::Enumeration,
    )]
    #[repr(i32)]
    pub enum Action {
        /// Unspecified.
        Unspecified = 0,
        /// Addition of an audit configuration.
        Add = 1,
        /// Removal of an audit configuration.
        Remove = 2,
    }
}
#[doc = r" Generated client implementations."]
pub mod iam_policy_client {
    #![allow(unused_variables, dead_code, missing_docs)]
    use tonic::codegen::*;
    #[doc = " ## API Overview"]
    #[doc = ""]
    #[doc = " Manages Identity and Access Management (IAM) policies."]
    #[doc = ""]
    #[doc = " Any implementation of an API that offers access control features"]
    #[doc = " implements the google.iam.v1.IAMPolicy interface."]
    #[doc = ""]
    #[doc = " ## Data model"]
    #[doc = ""]
    #[doc = " Access control is applied when a principal (user or service account), takes"]
    #[doc = " some action on a resource exposed by a service. Resources, identified by"]
    #[doc = " URI-like names, are the unit of access control specification. Service"]
    #[doc = " implementations can choose the granularity of access control and the"]
    #[doc = " supported permissions for their resources."]
    #[doc = " For example one database service may allow access control to be"]
    #[doc = " specified only at the Table level, whereas another might allow access control"]
    #[doc = " to also be specified at the Column level."]
    #[doc = ""]
    #[doc = " ## Policy Structure"]
    #[doc = ""]
    #[doc = " See google.iam.v1.Policy"]
    #[doc = ""]
    #[doc = " This is intentionally not a CRUD style API because access control policies"]
    #[doc = " are created and deleted implicitly with the resources to which they are"]
    #[doc = " attached."]
    pub struct IamPolicyClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl IamPolicyClient<tonic::transport::Channel> {
        #[doc = r" Attempt to create a new client by connecting to a given endpoint."]
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: std::convert::TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> IamPolicyClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::ResponseBody: Body + HttpBody + Send + 'static,
        T::Error: Into<StdError>,
        <T::ResponseBody as HttpBody>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_interceptor(
            inner: T,
            interceptor: impl Into<tonic::Interceptor>,
        ) -> Self {
            let inner = tonic::client::Grpc::with_interceptor(inner, interceptor);
            Self { inner }
        }
        #[doc = " Sets the access control policy on the specified resource. Replaces any"]
        #[doc = " existing policy."]
        pub async fn set_iam_policy(
            &mut self,
            request: impl tonic::IntoRequest<
                ::google_iam_iam_policy::SetIamPolicyRequest,
            >,
        ) -> Result<tonic::Response<::google_iam::Policy>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/google.iam.v1.IAMPolicy/SetIamPolicy",
            );
            self.inner.unary(request.into_request(), path, codec).await
        }
        #[doc = " Gets the access control policy for a resource."]
        #[doc = " Returns an empty policy if the resource exists and does not have a policy"]
        #[doc = " set."]
        pub async fn get_iam_policy(
            &mut self,
            request: impl tonic::IntoRequest<
                ::google_iam_iam_policy::GetIamPolicyRequest,
            >,
        ) -> Result<tonic::Response<::google_iam::Policy>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/google.iam.v1.IAMPolicy/GetIamPolicy",
            );
            self.inner.unary(request.into_request(), path, codec).await
        }
        #[doc = " Returns permissions that a caller has on the specified resource."]
        #[doc = " If the resource does not exist, this will return an empty set of"]
        #[doc = " permissions, not a NOT_FOUND error."]
        #[doc = ""]
        #[doc = " Note: This operation is designed to be used for building permission-aware"]
        #[doc = " UIs and command-line tools, not for authorization checking. This operation"]
        #[doc = " may \"fail open\" without warning."]
        pub async fn test_iam_permissions(
            &mut self,
            request: impl tonic::IntoRequest<
                ::google_iam_iam_policy::TestIamPermissionsRequest,
            >,
        ) -> Result<
            tonic::Response<::google_iam_iam_policy::TestIamPermissionsResponse>,
            tonic::Status,
        > {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/google.iam.v1.IAMPolicy/TestIamPermissions",
            );
            self.inner.unary(request.into_request(), path, codec).await
        }
    }
    impl<T: Clone> Clone for IamPolicyClient<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
            }
        }
    }
    impl<T> std::fmt::Debug for IamPolicyClient<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "IamPolicyClient {{ ... }}")
        }
    }
}
