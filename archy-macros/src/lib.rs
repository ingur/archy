use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput, Fields, ItemImpl, ImplItem, FnArg, ReturnType, Pat};

/// Derive macro for Service structs - generates ServiceFactory implementation
///
/// ```ignore
/// #[derive(Service)]
/// struct PaymentService {
///     config: Res<Config>,
///     orders: Client<OrderService>,
/// }
/// ```
///
/// Generates:
/// ```ignore
/// impl ::archy::ServiceFactory for PaymentService {
///     fn create(app: &::archy::App) -> Self {
///         PaymentService {
///             config: app.extract(),
///             orders: app.extract(),
///         }
///     }
/// }
/// ```
#[proc_macro_derive(Service)]
pub fn derive_service(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            _ => return syn::Error::new_spanned(&input, "#[derive(Service)] only supports structs with named fields")
                .to_compile_error()
                .into(),
        },
        _ => return syn::Error::new_spanned(&input, "#[derive(Service)] only supports structs")
            .to_compile_error()
            .into(),
    };

    let field_inits = fields.iter().map(|f| {
        let field_name = f.ident.as_ref().unwrap();
        quote! { #field_name: app.extract() }
    });

    let expanded = quote! {
        impl ::archy::ServiceFactory for #name {
            fn create(app: &::archy::App) -> Self {
                #name {
                    #(#field_inits),*
                }
            }
        }
    };

    TokenStream::from(expanded)
}

/// Attribute macro for Service impl blocks - generates message enum, Service impl, and Client methods
///
/// ```ignore
/// #[service]
/// impl PaymentService {
///     pub async fn process(&self, amount: u32) -> String {
///         // ...
///     }
/// }
/// ```
///
/// Generates:
/// - Message enum with variants for each public async method
/// - impl Service for PaymentService
/// - Client extension trait + impl
#[proc_macro_attribute]
pub fn service(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemImpl);
    let service_name = match &*input.self_ty {
        syn::Type::Path(type_path) => type_path.path.segments.last().unwrap().ident.clone(),
        _ => return syn::Error::new_spanned(&input.self_ty, "#[service] must be applied to an impl block for a named type")
            .to_compile_error()
            .into(),
    };

    let msg_enum_name = format_ident!("{}Msg", service_name);
    let client_trait_name = format_ident!("{}Client", service_name);

    // Collect public async methods
    let mut methods = Vec::new();
    for item in &input.items {
        if let ImplItem::Fn(method) = item {
            let is_pub = matches!(method.vis, syn::Visibility::Public(_));
            let is_async = method.sig.asyncness.is_some();
            let has_self = method.sig.inputs.first().map_or(false, |arg| matches!(arg, FnArg::Receiver(_)));

            if is_pub && is_async && has_self {
                methods.push(method);
            }
        }
    }

    // Generate message enum variants
    let msg_variants = methods.iter().map(|method| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameters (skip &self)
        let params: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    let name = &pat_ident.ident;
                    let ty = &pat_type.ty;
                    return Some(quote! { #name: #ty });
                }
            }
            None
        }).collect();

        // Fire-and-forget optimization: skip respond field for unit returns
        if is_unit_return(&method.sig.output) {
            quote! {
                #variant_ident { #(#params),* }
            }
        } else {
            let return_type = match &method.sig.output {
                ReturnType::Default => quote! { () },
                ReturnType::Type(_, ty) => quote! { #ty },
            };
            quote! {
                #variant_ident { #(#params,)* respond: ::archy::tokio::sync::oneshot::Sender<#return_type> }
            }
        }
    });

    // Generate handle match arms
    let match_arms = methods.iter().map(|method| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameter names (skip &self)
        let param_names: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    return Some(&pat_ident.ident);
                }
            }
            None
        }).collect();

        let method_call = if param_names.is_empty() {
            quote! { self.#method_name().await }
        } else {
            quote! { self.#method_name(#(#param_names),*).await }
        };

        // Fire-and-forget optimization: no respond for unit returns
        if is_unit_return(&method.sig.output) {
            let param_pattern = if param_names.is_empty() {
                quote! {}
            } else {
                quote! { #(#param_names),* }
            };
            quote! {
                #msg_enum_name::#variant_ident { #param_pattern } => {
                    #method_call;
                }
            }
        } else {
            let param_pattern = if param_names.is_empty() {
                quote! { respond }
            } else {
                quote! { #(#param_names,)* respond }
            };
            quote! {
                #msg_enum_name::#variant_ident { #param_pattern } => {
                    let result = #method_call;
                    let _ = respond.send(result);
                }
            }
        }
    });

    // Generate client trait methods (using async fn for cleaner syntax)
    let client_trait_methods = methods.iter().map(|method| {
        let method_name = &method.sig.ident;

        // Get parameters with types (skip &self)
        let params: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    let name = &pat_ident.ident;
                    let ty = &pat_type.ty;
                    return Some(quote! { #name: #ty });
                }
            }
            None
        }).collect();

        // Get return type wrapped in Result
        let return_type = match &method.sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };

        quote! {
            async fn #method_name(&self, #(#params),*) -> ::std::result::Result<#return_type, ::archy::ServiceError>;
        }
    });

    // Generate client trait impl methods
    let client_impl_methods = methods.iter().map(|method| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameters with types (skip &self)
        let params: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    let name = &pat_ident.ident;
                    let ty = &pat_type.ty;
                    return Some((name.clone(), quote! { #ty }));
                }
            }
            None
        }).collect();

        let param_decls: Vec<_> = params.iter().map(|(name, ty)| quote! { #name: #ty }).collect();
        let param_names: Vec<_> = params.iter().map(|(name, _)| name).collect();

        // Get return type
        let return_type = match &method.sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };

        // Fire-and-forget optimization for unit returns
        if is_unit_return(&method.sig.output) {
            let msg_construction = if param_names.is_empty() {
                quote! { #msg_enum_name::#variant_ident {} }
            } else {
                quote! { #msg_enum_name::#variant_ident { #(#param_names),* } }
            };

            quote! {
                async fn #method_name(&self, #(#param_decls),*) -> ::std::result::Result<#return_type, ::archy::ServiceError> {
                    self.sender.send(#msg_construction).await
                        .map_err(|_| ::archy::ServiceError::ChannelClosed)?;
                    Ok(())
                }
            }
        } else {
            let msg_construction = if param_names.is_empty() {
                quote! { #msg_enum_name::#variant_ident { respond: tx } }
            } else {
                quote! { #msg_enum_name::#variant_ident { #(#param_names,)* respond: tx } }
            };

            quote! {
                async fn #method_name(&self, #(#param_decls),*) -> ::std::result::Result<#return_type, ::archy::ServiceError> {
                    let (tx, rx) = ::archy::tokio::sync::oneshot::channel();
                    self.sender.send(#msg_construction).await
                        .map_err(|_| ::archy::ServiceError::ChannelClosed)?;
                    rx.await.map_err(|_| ::archy::ServiceError::ServiceDropped)
                }
            }
        }
    });

    let expanded = quote! {
        // Original impl block (preserved)
        #input

        // Generated message enum
        pub enum #msg_enum_name {
            #(#msg_variants),*
        }

        // Generated Service implementation
        impl ::archy::Service for #service_name {
            type Message = #msg_enum_name;

            fn create(app: &::archy::App) -> Self {
                <Self as ::archy::ServiceFactory>::create(app)
            }

            fn handle(self: ::std::sync::Arc<Self>, msg: Self::Message) -> impl ::std::future::Future<Output = ()> + Send {
                async move {
                    match msg {
                        #(#match_arms)*
                    }
                }
            }
        }

        // Generated client trait
        #[allow(async_fn_in_trait)]
        pub trait #client_trait_name {
            #(#client_trait_methods)*
        }

        // Generated client impl
        impl #client_trait_name for ::archy::Client<#service_name> {
            #(#client_impl_methods)*
        }
    };

    TokenStream::from(expanded)
}

fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Check if a return type is unit () - used for fire-and-forget optimization
fn is_unit_return(output: &ReturnType) -> bool {
    match output {
        ReturnType::Default => true,
        ReturnType::Type(_, ty) => {
            if let syn::Type::Tuple(tuple) = &**ty {
                tuple.elems.is_empty()
            } else {
                false
            }
        }
    }
}
