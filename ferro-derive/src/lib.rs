//! The derive macros behind `ferro::Data` and `ferro::Record`.
//!
//! Use them through the `ferro` crate, which re-exports both, as `use ferro::{Data, Record};`.
//! The code they generate names `::ferro`, so the crate must be a dependency under that name.
//!
//! - [`Data`](derive@Data) encodes a struct or an enum by field name, so the type can change while
//!   the data it wrote keeps loading.
//! - [`Record`](derive@Record) makes a struct a row of `ferro::Table`, generating the secondary
//!   indexes its attributes declare and the lookups on them.

#![warn(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Fields, Generics, Ident, LitStr, Path, Type, Visibility,
    parse_macro_input,
};

fn field_tag(name: &str) -> u16 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    ((hash >> 16) ^ (hash & 0xffff)) as u16
}

enum Fallback {
    Required,
    Default,
    With(Path),
    Skip,
}

struct Field {
    ident: Option<Ident>,
    ty: Type,
    name: String,
    span: Span,
    fallback: Fallback,
}

impl Field {
    fn stored(&self) -> bool {
        !matches!(self.fallback, Fallback::Skip)
    }

    fn tag(&self) -> u16 {
        field_tag(&self.name)
    }

    fn default_value(&self) -> Option<TokenStream2> {
        let ty = &self.ty;
        match &self.fallback {
            Fallback::Default => Some(quote! { <#ty as ::std::default::Default>::default() }),
            Fallback::With(path) => Some(quote! { #path() }),
            Fallback::Required | Fallback::Skip => None,
        }
    }
}

struct Attrs {
    rename: Option<String>,
    fallback: Fallback,
    removed: Vec<LitStr>,
}

fn removed_list(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<Vec<LitStr>> {
    let content;
    syn::parenthesized!(content in meta.input);
    let names = syn::punctuated::Punctuated::<LitStr, syn::Token![,]>::parse_terminated(&content)?;
    if names.is_empty() {
        return Err(meta.error("removed needs at least one name"));
    }
    Ok(names.into_iter().collect())
}

fn parse_attrs(attrs: &[Attribute]) -> syn::Result<Attrs> {
    let mut out = Attrs {
        rename: None,
        fallback: Fallback::Required,
        removed: Vec::new(),
    };
    for attr in attrs {
        if !attr.path().is_ident("ferro") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let lit: LitStr = meta.value()?.parse()?;
                out.rename = Some(lit.value());
                Ok(())
            } else if meta.path.is_ident("skip") {
                out.fallback = Fallback::Skip;
                Ok(())
            } else if meta.path.is_ident("default") {
                if meta.input.peek(syn::Token![=]) {
                    let lit: LitStr = meta.value()?.parse()?;
                    out.fallback = Fallback::With(lit.parse()?);
                } else {
                    out.fallback = Fallback::Default;
                }
                Ok(())
            } else if meta.path.is_ident("removed") {
                out.removed.extend(removed_list(&meta)?);
                Ok(())
            } else {
                Err(meta.error("unknown ferro attribute"))
            }
        })?;
    }
    Ok(out)
}

fn parse_fields(fields: &Fields) -> syn::Result<Vec<Field>> {
    let mut out = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let attrs = parse_attrs(&field.attrs)?;
        if let Some(first) = attrs.removed.first() {
            return Err(syn::Error::new(
                first.span(),
                "removed lists the names a struct, an enum or a variant no longer has, so it goes on the struct, the enum or the variant",
            ));
        }
        let name = attrs.rename.unwrap_or_else(|| match &field.ident {
            Some(ident) => ident.to_string(),
            None => index.to_string(),
        });
        out.push(Field {
            ident: field.ident.clone(),
            ty: field.ty.clone(),
            name,
            span: field.span(),
            fallback: attrs.fallback,
        });
    }
    Ok(out)
}

fn check_collisions<'a>(
    kind: &str,
    items: impl Iterator<Item = (&'a str, u16, Span)>,
    removed: &[LitStr],
) -> syn::Result<()> {
    let mut seen: Vec<(&str, u16, Span)> = Vec::new();
    for (name, tag, span) in items {
        if let Some(gone) = removed.iter().find(|gone| field_tag(&gone.value()) == tag) {
            let gone_name = gone.value();
            let message = if gone_name == name {
                format!(
                    "{kind} `{name}` is listed as removed; take it off the list to bring it back, or store it under another name with #[ferro(rename = \"...\")]"
                )
            } else {
                format!(
                    "{kind} `{name}` has the same tag ({tag}) as removed {kind} `{gone_name}`, so it would read what old data stored under `{gone_name}`; store it under another name with #[ferro(rename = \"...\")]"
                )
            };
            let mut err = syn::Error::new(span, message);
            err.combine(syn::Error::new(
                gone.span(),
                format!("removed {kind} `{gone_name}` is listed here"),
            ));
            return Err(err);
        }
        if let Some((other, _, other_span)) = seen.iter().find(|(_, t, _)| *t == tag) {
            let mut err = syn::Error::new(
                span,
                format!(
                    "{kind} `{name}` has the same tag ({tag}) as {kind} `{other}`; use #[ferro(rename = \"...\")] on one of them"
                ),
            );
            err.combine(syn::Error::new(
                *other_span,
                format!("{kind} `{other}` collides with `{name}`"),
            ));
            return Err(err);
        }
        seen.push((name, tag, span));
    }
    Ok(())
}

fn check_fields(fields: &[Field], removed: &[LitStr]) -> syn::Result<()> {
    check_collisions(
        "field",
        fields
            .iter()
            .filter(|f| f.stored())
            .map(|f| (f.name.as_str(), f.tag(), f.span)),
        removed,
    )
}

fn encode_stmts(fields: &[Field], access: &[TokenStream2]) -> TokenStream2 {
    let mut stored: Vec<(u16, &TokenStream2)> = fields
        .iter()
        .zip(access)
        .filter(|(f, _)| f.stored())
        .map(|(f, a)| (f.tag(), a))
        .collect();
    stored.sort_by_key(|(tag, _)| *tag);
    let count = stored.len() as u64;
    let writes = stored.iter().map(|(tag, a)| {
        quote! { __w.field(#tag, |__w| ::ferro::Data::encode(#a, __w))?; }
    });
    quote! {
        __w.varint(#count);
        #(#writes)*
    }
}

fn decode_stmts(fields: &[Field], vars: &[Ident]) -> (TokenStream2, Vec<TokenStream2>) {
    let mut decls = Vec::new();
    let mut arms = Vec::new();
    let mut values = Vec::new();
    for (field, var) in fields.iter().zip(vars) {
        let name = &field.name;
        let value = match &field.fallback {
            Fallback::Skip => quote! { ::std::default::Default::default() },
            Fallback::Required => {
                quote! { ::ferro::codec::required(#var, #name)? }
            }
            Fallback::Default => quote! {
                match #var {
                    ::std::option::Option::Some(__v) => __v,
                    ::std::option::Option::None => ::std::default::Default::default(),
                }
            },
            Fallback::With(path) => quote! {
                match #var {
                    ::std::option::Option::Some(__v) => __v,
                    ::std::option::Option::None => #path(),
                }
            },
        };
        if field.stored() {
            let tag = field.tag();
            decls.push(quote! { let mut #var = ::std::option::Option::None; });
            arms.push(quote! {
                #tag => #var = ::std::option::Option::Some(::ferro::Data::decode(__r)?),
            });
        }
        values.push(value);
    }
    let stmts = quote! {
        #(#decls)*
        __r.fields(|__tag, __r| {
            match __tag {
                #(#arms)*
                _ => {}
            }
            ::std::result::Result::Ok(())
        })?;
    };
    (stmts, values)
}

fn visit_stmts(fields: &[Field]) -> TokenStream2 {
    let stored: Vec<&Field> = fields.iter().filter(|f| f.stored()).collect();
    let seen: Vec<Ident> = (0..stored.len())
        .map(|i| format_ident!("__seen{}", i))
        .collect();
    let arms = stored.iter().zip(&seen).map(|(field, seen)| {
        let tag = field.tag();
        let ty = &field.ty;
        quote! {
            #tag => {
                #seen = true;
                <#ty as ::ferro::Data>::visit(__r)?;
            }
        }
    });
    let required = stored
        .iter()
        .zip(&seen)
        .filter(|(field, _)| matches!(field.fallback, Fallback::Required))
        .map(|(field, seen)| {
            let ty = &field.ty;
            let name = &field.name;
            quote! {
                if !#seen && <#ty as ::ferro::Data>::missing().is_none() {
                    return ::std::result::Result::Err(::ferro::Error::MissingField(#name));
                }
            }
        });
    quote! {
        #(let mut #seen = false;)*
        __r.fields(|__tag, __r| {
            match __tag {
                #(#arms)*
                _ => {}
            }
            ::std::result::Result::Ok(())
        })?;
        #(#required)*
    }
}

fn describe_stmts(fields: &[Field]) -> TokenStream2 {
    let mut stored: Vec<&Field> = fields.iter().filter(|f| f.stored()).collect();
    stored.sort_by(|a, b| (a.tag(), &a.name).cmp(&(b.tag(), &b.name)));
    let count = stored.len() as u64;
    let parts = stored.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let default = field.default_value().map(|value| {
            quote! {
                __d.word("default");
                __d.value::<#ty>(&#value);
            }
        });
        quote! {
            __d.word(#name);
            __d.of::<#ty>();
            #default
        }
    });
    quote! {
        __d.count(#count);
        #(#parts)*
    }
}

fn construct(
    path: TokenStream2,
    fields: &[Field],
    shape: &Fields,
    values: &[TokenStream2],
) -> TokenStream2 {
    match shape {
        Fields::Named(_) => {
            let idents = fields.iter().map(|f| f.ident.as_ref().unwrap());
            quote! { #path { #(#idents: #values),* } }
        }
        Fields::Unnamed(_) => quote! { #path(#(#values),*) },
        Fields::Unit => path,
    }
}

fn bound_generics(generics: &Generics) -> Generics {
    let mut generics = generics.clone();
    for param in generics.type_params_mut() {
        param.bounds.push(syn::parse_quote!(::ferro::Data));
    }
    generics
}

fn vars(count: usize) -> Vec<Ident> {
    (0..count).map(|i| format_ident!("__f{}", i)).collect()
}

struct Container {
    transparent: bool,
    removed: Vec<LitStr>,
}

fn parse_container(attrs: &[Attribute]) -> syn::Result<Container> {
    let mut out = Container {
        transparent: false,
        removed: Vec::new(),
    };
    for attr in attrs {
        if !attr.path().is_ident("ferro") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                out.transparent = true;
                Ok(())
            } else if meta.path.is_ident("removed") {
                out.removed.extend(removed_list(&meta)?);
                Ok(())
            } else {
                Err(meta.error("unknown ferro attribute"))
            }
        })?;
    }
    if let (true, Some(first)) = (out.transparent, out.removed.first()) {
        return Err(syn::Error::new(
            first.span(),
            "a transparent struct stores its field without a tag, so it has no removed names",
        ));
    }
    Ok(out)
}

fn expand_transparent(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let generics = bound_generics(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "transparent needs a struct with one field",
        ));
    };
    let fields = parse_fields(&data.fields)?;
    let [field] = fields.as_slice() else {
        return Err(syn::Error::new(
            input.span(),
            "transparent needs a struct with one field",
        ));
    };
    let (access, other) = match &field.ident {
        Some(ident) => (quote! { &self.#ident }, quote! { &__other.#ident }),
        None => (quote! { &self.0 }, quote! { &__other.0 }),
    };
    let build = construct(quote! { Self }, &fields, &data.fields, &[quote! { __v }]);
    let ty = &field.ty;
    Ok(quote! {
        impl #impl_generics ::ferro::Data for #name #ty_generics #where_clause {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                ::ferro::Data::encode(#access, __w)
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                let __v = ::ferro::Data::decode(__r)?;
                ::std::result::Result::Ok(#build)
            }

            fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                <#ty as ::ferro::Data>::visit(__r)
            }

            fn missing() -> ::std::option::Option<Self> {
                ::ferro::Data::missing().map(|__v| #build)
            }

            fn describe(__d: &mut ::ferro::codec::Description) {
                __d.named::<Self>(|__d| __d.of::<#ty>());
            }

            fn same_encoding(&self, __other: &Self) -> bool {
                ::ferro::Data::same_encoding(#access, #other)
            }
        }
    })
}

fn expand_data(input: DeriveInput) -> syn::Result<TokenStream2> {
    let container = parse_container(&input.attrs)?;
    if container.transparent {
        return expand_transparent(&input);
    }
    let name = &input.ident;
    let generics = bound_generics(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let (encode, decode, visit, describe, same) = match &input.data {
        Data::Struct(data) => {
            let fields = parse_fields(&data.fields)?;
            check_fields(&fields, &container.removed)?;
            let members: Vec<TokenStream2> = data
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| match &f.ident {
                    Some(ident) => quote! { &self.#ident },
                    None => {
                        let index = syn::Index::from(i);
                        quote! { &self.#index }
                    }
                })
                .collect();
            let same = data
                .fields
                .iter()
                .zip(&fields)
                .enumerate()
                .filter(|(_, (_, field))| field.stored())
                .map(|(i, (f, _))| {
                    let member = match &f.ident {
                        Some(ident) => quote! { #ident },
                        None => {
                            let index = syn::Index::from(i);
                            quote! { #index }
                        }
                    };
                    quote! { && ::ferro::Data::same_encoding(&self.#member, &__other.#member) }
                });
            let same = quote! { true #(#same)* };
            let encode = encode_stmts(&fields, &members);
            let vs = vars(fields.len());
            let (stmts, values) = decode_stmts(&fields, &vs);
            let build = construct(quote! { Self }, &fields, &data.fields, &values);
            let visit = visit_stmts(&fields);
            let described = describe_stmts(&fields);
            (
                quote! { #encode ::std::result::Result::Ok(()) },
                quote! { #stmts ::std::result::Result::Ok(#build) },
                quote! { #visit ::std::result::Result::Ok(()) },
                quote! {
                    __d.word("struct");
                    #described
                },
                same,
            )
        }
        Data::Enum(data) => {
            let mut variant_names = Vec::new();
            let mut removed_fields = Vec::new();
            for variant in &data.variants {
                let attrs = parse_attrs(&variant.attrs)?;
                variant_names.push(attrs.rename.unwrap_or_else(|| variant.ident.to_string()));
                removed_fields.push(attrs.removed);
            }
            check_collisions(
                "variant",
                variant_names
                    .iter()
                    .zip(&data.variants)
                    .map(|(n, v)| (n.as_str(), field_tag(n), v.span())),
                &container.removed,
            )?;
            let mut encode_arms = Vec::new();
            let mut decode_arms = Vec::new();
            let mut visit_arms = Vec::new();
            let mut same_arms = Vec::new();
            let mut described = Vec::new();
            for ((variant, vname), removed) in data
                .variants
                .iter()
                .zip(&variant_names)
                .zip(&removed_fields)
            {
                let vident = &variant.ident;
                let vtag = field_tag(vname);
                let fields = parse_fields(&variant.fields)?;
                check_fields(&fields, removed)?;
                let vs = vars(fields.len());
                let others: Vec<Ident> = (0..fields.len())
                    .map(|i| format_ident!("__g{}", i))
                    .collect();
                let bind = |vs: &[Ident]| match &variant.fields {
                    Fields::Named(_) => {
                        let binds = fields.iter().zip(vs).map(|(f, v)| {
                            let ident = f.ident.as_ref().unwrap();
                            if f.stored() {
                                quote! { #ident: #v }
                            } else {
                                quote! { #ident: _ }
                            }
                        });
                        quote! { Self::#vident { #(#binds),* } }
                    }
                    Fields::Unnamed(_) => {
                        let binds = fields.iter().zip(vs).map(|(f, v)| {
                            if f.stored() {
                                quote! { #v }
                            } else {
                                quote! { _ }
                            }
                        });
                        quote! { Self::#vident(#(#binds),*) }
                    }
                    Fields::Unit => quote! { Self::#vident },
                };
                let pattern = bind(&vs);
                let other_pattern = bind(&others);
                let same = fields
                    .iter()
                    .zip(vs.iter().zip(&others))
                    .filter(|(field, _)| field.stored())
                    .map(|(_, (v, o))| quote! { && ::ferro::Data::same_encoding(#v, #o) });
                same_arms.push(quote! {
                    (#pattern, #other_pattern) => true #(#same)*,
                });
                let access: Vec<TokenStream2> = vs.iter().map(|v| quote! { #v }).collect();
                let body = encode_stmts(&fields, &access);
                encode_arms.push(quote! {
                    #pattern => {
                        __w.u16(#vtag);
                        #body
                    }
                });
                let (stmts, values) = decode_stmts(&fields, &vs);
                let build = construct(quote! { Self::#vident }, &fields, &variant.fields, &values);
                decode_arms.push(quote! {
                    #vtag => {
                        #stmts
                        ::std::result::Result::Ok(#build)
                    }
                });
                let visit = visit_stmts(&fields);
                visit_arms.push(quote! {
                    #vtag => {
                        #visit
                        ::std::result::Result::Ok(())
                    }
                });
                let fields_described = describe_stmts(&fields);
                described.push((
                    vtag,
                    vname.clone(),
                    quote! {
                        __d.word(#vname);
                        #fields_described
                    },
                ));
            }
            described.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            let variant_count = described.len() as u64;
            let variants_described = described.iter().map(|(_, _, tokens)| tokens);
            let encode_match = if data.variants.is_empty() {
                quote! { match *self {} }
            } else {
                quote! {
                    match self { #(#encode_arms)* }
                    ::std::result::Result::Ok(())
                }
            };
            let same_match = if data.variants.is_empty() {
                quote! { match *self {} }
            } else {
                quote! {
                    match (self, __other) {
                        #(#same_arms)*
                        _ => false,
                    }
                }
            };
            let unknown = quote! {
                __other => ::std::result::Result::Err(::ferro::Error::Decode(
                    ::std::format!("unknown variant tag {} for {}", __other, ::std::stringify!(#name))
                )),
            };
            (
                encode_match,
                quote! {
                    let __variant = __r.u16()?;
                    match __variant {
                        #(#decode_arms)*
                        #unknown
                    }
                },
                quote! {
                    let __variant = __r.u16()?;
                    match __variant {
                        #(#visit_arms)*
                        #unknown
                    }
                },
                quote! {
                    __d.word("enum");
                    __d.count(#variant_count);
                    #(#variants_described)*
                },
                same_match,
            )
        }
        Data::Union(_) => {
            return Err(syn::Error::new(
                input.span(),
                "Data cannot be derived for unions",
            ));
        }
    };
    Ok(quote! {
        impl #impl_generics ::ferro::Data for #name #ty_generics #where_clause {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                #encode
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                #decode
            }

            fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                #visit
            }

            fn describe(__d: &mut ::ferro::codec::Description) {
                __d.named::<Self>(|__d| {
                    #describe
                });
            }

            #[allow(unreachable_patterns)]
            fn same_encoding(&self, __other: &Self) -> bool {
                #same
            }
        }
    })
}

/// Derives `ferro::Data`, the encoding every stored value needs, for a struct or an enum.
///
/// Use it through the re-export, `use ferro::Data;`. The type must also be `Clone`, and every
/// field's type must implement `Data`. Every type parameter gets a `Data` bound. Unions are
/// refused.
///
/// # Encoding
///
/// A struct is stored as its fields, each under a 16-bit tag hashed from its name and sorted by
/// tag, so the struct can change while old data keeps loading:
///
/// - Reordering fields changes nothing, not even the value's hash.
/// - A field the type no longer has is skipped when old data is read, unless a field added later
///   has the same tag; listing the removed name in `#[ferro(removed(...))]` rules that out.
/// - A field that old data lacks reads as its type's empty value if it has one: `Option`, `Vec`,
///   `BTreeMap`, `BTreeSet`, `Bytes`, `Map`, `Set`, `List` and `Table` start empty, and `Box` and
///   `Arc` follow what they hold. Any other field needs `#[ferro(default)]`, or the read fails with
///   `Error::MissingField`.
///
/// Tuple fields are named by position, `0`, `1` and so on, and a unit struct stores no fields. An
/// enum stores a tag hashed from the variant's name, then the variant's fields as a struct's, so
/// variants can be reordered and added. A variant tag the type does not know fails to decode with
/// `Error::Decode`, as long as no variant added later has the tag of the removed one.
///
/// Do not change the encoding or `Ord` of a type used as a key or an indexed value on existing
/// data.
/// [The schema evolution guide](https://github.com/archlemon/ferro/blob/main/docs/guide/schema-evolution.md)
/// has the rules.
///
/// # Attributes
///
/// On a field, of a struct or of an enum variant:
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `#[ferro(rename = "name")]` | Stores the field under `name`, so a field renamed in Rust keeps reading what it wrote before. |
/// | `#[ferro(default)]` | Data that lacks the field reads it as `Default::default()`. |
/// | `#[ferro(default = "path::to::function")]` | Data that lacks the field reads it as `path::to::function()`. |
/// | `#[ferro(skip)]` | The field is not stored, and every read gives `Default::default()`. |
///
/// On an enum variant, `#[ferro(rename = "name")]` stores the variant under `name`.
///
/// On a struct, an enum or an enum variant, `#[ferro(removed("name", ...))]` lists the stored
/// names of the fields, or on an enum the variants, that it once had and no longer has. A tag is
/// 16 bits, so a field added later can hash to the tag of one that is gone, and would then read the
/// bytes old data stored under it: the old value if they decode as the new field's type, and a
/// failed read if not. The derive refuses a field or variant whose stored name has the tag of a
/// listed name, so that `#[ferro(rename = "...")]` can give it another, and refuses a listed name
/// that the type still has. Names nobody lists are not checked: list every field or variant you
/// remove from a type that has stored data.
///
/// On a struct with exactly one field, `#[ferro(transparent)]` stores the struct as that field
/// alone, with no tag, and describes it as that field, so a newtype costs nothing over what it
/// wraps and wrapping a field in one leaves its encoding and its indexes as they were.
///
/// Options can share one attribute, as in `#[ferro(rename = "total", default)]`. Any other option
/// is a compile error, and so are two fields, or two variants, whose stored names hash to the same
/// tag: the error names both, and `#[ferro(rename = "...")]` on one of them resolves it. A field or
/// variant whose tag is that of a removed name is refused the same way:
///
/// ```compile_fail
/// use ferro::Data;
///
/// #[derive(Data, Clone)]
/// #[ferro(removed("title_score"))]
/// struct Post {
///     id: u32,
///     #[ferro(default)]
///     flags: u32,
/// }
/// ```
///
/// # Examples
///
/// Reordering, renaming and skipping fields leave the stored value as it was:
///
/// ```
/// use ferro::{Data, hash_of};
///
/// #[derive(Data, Clone)]
/// struct Before {
///     owner: String,
///     balance: i64,
/// }
///
/// #[derive(Data, Clone)]
/// struct After {
///     #[ferro(rename = "balance")]
///     balance_cents: i64,
///     owner: String,
///     #[ferro(skip)]
///     label: String,
/// }
///
/// let before = Before { owner: "ana".into(), balance: 5 };
/// let after = After { balance_cents: 5, owner: "ana".into(), label: "cached".into() };
/// assert_eq!(hash_of(&before), hash_of(&after));
/// ```
///
/// A field that old data lacks reads as its default, and a newtype stores what it wraps:
///
/// ```
/// use ferro::codec::{Decoder, Encoder, HashSink};
/// use ferro::{Data, hash_of};
///
/// fn reread<A: Data, B: Data>(value: &A) -> B {
///     let mut sink = HashSink;
///     let mut encoder = Encoder::new(&mut sink);
///     value.encode(&mut encoder).unwrap();
///     let body = encoder.into_object().body;
///     B::decode(&mut Decoder::new(&body, &[], None)).unwrap()
/// }
///
/// #[derive(Data, Clone)]
/// struct V1 {
///     name: String,
/// }
///
/// #[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// #[ferro(transparent)]
/// struct Cents(i64);
///
/// #[derive(Data, Clone, Debug, PartialEq)]
/// enum Plan {
///     Free,
///     #[ferro(rename = "Pro")]
///     Paid { seats: u32 },
/// }
///
/// #[derive(Data, Clone, Debug)]
/// struct V2 {
///     name: String,
///     nickname: Option<String>,
///     #[ferro(default)]
///     active: bool,
///     #[ferro(default = "V2::starting_limit")]
///     limit: Cents,
///     #[ferro(default = "V2::free")]
///     plan: Plan,
/// }
///
/// impl V2 {
///     fn starting_limit() -> Cents {
///         Cents(10_000)
///     }
///
///     fn free() -> Plan {
///         Plan::Free
///     }
/// }
///
/// let v2: V2 = reread(&V1 { name: "ana".into() });
/// assert_eq!(v2.nickname, None);
/// assert!(!v2.active);
/// assert_eq!(v2.limit, Cents(10_000));
/// assert_eq!(v2.plan, Plan::Free);
/// assert_eq!(hash_of(&Cents(7)), hash_of(&7i64));
/// ```
#[proc_macro_derive(Data, attributes(ferro))]
pub fn derive_data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_data(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

enum IndexKind {
    Value,
    Each,
    Unique,
}

fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

struct Indexed {
    name: String,
    span: Span,
    fields: Vec<(Ident, Type)>,
    kind: IndexKind,
    cover: Vec<(Ident, Type)>,
}

const MAX_STORED: usize = 12;

impl Indexed {
    fn indexes(&self, ident: &Ident) -> Option<usize> {
        self.fields.iter().position(|(field, _)| field == ident)
    }

    fn stored(&self) -> Vec<&(Ident, Type)> {
        self.cover
            .iter()
            .filter(|(ident, _)| self.indexes(ident).is_none())
            .collect()
    }

    fn stores_cover(&self) -> bool {
        !self.stored().is_empty()
    }

    fn stored_ty(&self) -> TokenStream2 {
        let tys = self.stored().into_iter().map(|(_, ty)| ty);
        quote! { (#(#tys,)*) }
    }

    fn cover_ty(&self) -> TokenStream2 {
        let tys: Vec<&Type> = self.cover.iter().map(|(_, ty)| ty).collect();
        match tys.as_slice() {
            [only] => quote! { #only },
            _ => quote! { (#(#tys),*) },
        }
    }

    fn stored_of(&self) -> TokenStream2 {
        let clones = self
            .stored()
            .into_iter()
            .map(|(ident, _)| quote! { ::std::clone::Clone::clone(&__r.#ident) });
        quote! { (#(#clones,)*) }
    }

    fn same_cover(&self) -> TokenStream2 {
        let same = self
            .stored()
            .into_iter()
            .map(|(ident, _)| quote! { ::ferro::table::same_stored(&__o.#ident, &__n.#ident) });
        quote! { true #(&& #same)* }
    }

    fn kind_word(&self) -> &'static str {
        let composite = self.fields.len() > 1;
        let option = self
            .sole()
            .is_some_and(|(_, ty)| option_inner(ty).is_some());
        match self.kind {
            IndexKind::Value if composite => "composite",
            IndexKind::Value if option => "option",
            IndexKind::Value => "value",
            IndexKind::Each => "each",
            IndexKind::Unique if composite => "unique composite",
            IndexKind::Unique if option => "unique option",
            IndexKind::Unique => "unique",
        }
    }

    fn fingerprint(
        &self,
        key_ty: &Type,
        stored_name: &dyn Fn(&Ident) -> String,
        default_of: &dyn Fn(&Ident) -> Option<TokenStream2>,
    ) -> TokenStream2 {
        let kind = self.kind_word();
        let described = |fields: Vec<&(Ident, Type)>| {
            let count = fields.len() as u64;
            let parts = fields.into_iter().map(|(ident, ty)| {
                let name = stored_name(ident);
                let default = default_of(ident).map(|value| {
                    quote! {
                        __d.word("default");
                        __d.value::<#ty>(&#value);
                    }
                });
                quote! {
                    __d.word(#name);
                    __d.of::<#ty>();
                    #default
                }
            });
            quote! {
                __d.count(#count);
                #(#parts)*
            }
        };
        let keyed = described(self.fields.iter().collect());
        let covered = described(self.stored());
        quote! {
            ::ferro::table::index_fingerprint(|__d| {
                __d.word(#kind);
                __d.of::<#key_ty>();
                #keyed
                #covered
            })
        }
    }

    fn projected(&self) -> TokenStream2 {
        let mut parts = Vec::new();
        let mut slot = 0;
        for (ident, _) in &self.cover {
            parts.push(match self.indexes(ident) {
                Some(_) if self.fields.len() == 1 => {
                    quote! { ::std::clone::Clone::clone(__value) }
                }
                Some(position) => {
                    let position = syn::Index::from(position);
                    quote! { ::std::clone::Clone::clone(&__value.#position) }
                }
                None => {
                    let position = syn::Index::from(slot);
                    slot += 1;
                    quote! { __stored.#position }
                }
            });
        }
        match parts.as_slice() {
            [only] => quote! { #only },
            _ => quote! { (#(#parts),*) },
        }
    }

    fn project_fn(&self) -> Ident {
        format_ident!("__{}_cover", self.field())
    }

    fn values_fn(&self) -> Ident {
        format_ident!("__{}_values", self.field())
    }

    fn values(&self) -> TokenStream2 {
        if let (IndexKind::Each, Some((ident, _))) = (&self.kind, self.sole()) {
            return quote! {
                ::ferro::table::IndexEach::values(&__r.#ident).iter().any(|__v| __f(__v))
            };
        }
        if let (Some(_), Some((ident, _))) = (self.sparse(), self.sole()) {
            return quote! {
                match &__r.#ident {
                    ::std::option::Option::Some(__v) => __f(__v),
                    ::std::option::Option::None => false,
                }
            };
        }
        match self.sole() {
            Some((ident, _)) => quote! { __f(&__r.#ident) },
            None => {
                let owned = self.owned();
                quote! { __f(&#owned) }
            }
        }
    }

    fn project_path(&self) -> TokenStream2 {
        if self.cover.is_empty() {
            let vt = self.value_ty();
            quote! { ::ferro::table::plain::<#vt> }
        } else {
            let project = self.project_fn();
            quote! { Self::#project }
        }
    }

    fn found_path(&self, table: &Ident) -> TokenStream2 {
        if self.cover.is_empty() {
            let vt = self.value_ty();
            quote! { ::ferro::table::plain::<#vt> }
        } else {
            let project = self.project_fn();
            quote! { #table::#project }
        }
    }

    fn field(&self) -> Ident {
        match self.kind {
            IndexKind::Unique => format_ident!("unique_{}", self.name),
            _ => format_ident!("by_{}", self.name),
        }
    }

    fn method(&self) -> Ident {
        format_ident!("by_{}", self.name)
    }

    fn sole(&self) -> Option<&(Ident, Type)> {
        match self.fields.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    fn sparse(&self) -> Option<&Type> {
        match self.kind {
            IndexKind::Unique => self.sole().and_then(|(_, ty)| option_inner(ty)),
            _ => None,
        }
    }

    fn value_ty(&self) -> TokenStream2 {
        if let Some(inner) = self.sparse() {
            return quote! { #inner };
        }
        let tys = self.fields.iter().map(|(_, ty)| ty);
        match (&self.kind, self.sole()) {
            (IndexKind::Each, Some((_, ty))) => {
                quote! { <#ty as ::ferro::table::IndexEach>::Value }
            }
            (_, Some((_, ty))) => quote! { #ty },
            _ => quote! { (#(#tys),*) },
        }
    }

    fn owned(&self) -> TokenStream2 {
        let clones = self
            .fields
            .iter()
            .map(|(ident, _)| quote! { ::std::clone::Clone::clone(&__r.#ident) });
        match self.sole() {
            Some(_) => quote! { #(#clones)* },
            None => quote! { (#(#clones),*) },
        }
    }

    fn optional(&self) -> TokenStream2 {
        let owned = self.owned();
        if self.sparse().is_some() {
            owned
        } else {
            quote! { ::std::option::Option::Some(#owned) }
        }
    }

    fn subject(&self) -> String {
        let names: Vec<String> = self
            .fields
            .iter()
            .map(|(ident, _)| ident.to_string())
            .collect();
        match names.as_slice() {
            [only] => format!("`{only}`"),
            _ => format!("`({})`", names.join(", ")),
        }
    }

    fn rest(&self) -> String {
        let names: Vec<String> = self.fields[1..]
            .iter()
            .map(|(ident, _)| ident.to_string())
            .collect();
        match names.as_slice() {
            [only] => format!("`{only}`"),
            more => format!("`({})`", more.join(", ")),
        }
    }

    fn matching(&self) -> String {
        let subject = self.subject();
        match self.kind {
            IndexKind::Each => format!("whose {subject} holds `value`"),
            IndexKind::Unique if self.sparse().is_some() => {
                format!("whose {subject} is `Some(value)`")
            }
            _ => format!("whose {subject} is `value`"),
        }
    }

    fn covers(&self) -> String {
        let names: Vec<String> = self
            .cover
            .iter()
            .map(|(ident, _)| format!("`{ident}`"))
            .collect();
        let list = match names.split_last() {
            None => return String::new(),
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        };
        format!(
            "\n\nThe index covers {list}, which `.covered()` yields with each key without reading the rows."
        )
    }
}

fn field_named(fields: &syn::FieldsNamed, name: &Ident) -> syn::Result<(Ident, Type)> {
    fields
        .named
        .iter()
        .find(|field| field.ident.as_ref() == Some(name))
        .map(|field| (name.clone(), field.ty.clone()))
        .ok_or_else(|| syn::Error::new(name.span(), format!("no field named `{name}`")))
}

fn cover_list(
    meta: &syn::meta::ParseNestedMeta<'_>,
    fields: &syn::FieldsNamed,
) -> syn::Result<Vec<(Ident, Type)>> {
    let content;
    syn::parenthesized!(content in meta.input);
    let names = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated(&content)?;
    if names.is_empty() {
        return Err(meta.error("cover needs at least one field"));
    }
    names.iter().map(|name| field_named(fields, name)).collect()
}

fn is_cover(meta: &syn::meta::ParseNestedMeta<'_>) -> bool {
    meta.path.is_ident("cover") && meta.input.peek(syn::token::Paren)
}

type Composite = (Vec<(Ident, Type)>, Vec<(Ident, Type)>);

fn composite(attr: &Attribute, fields: &syn::FieldsNamed) -> syn::Result<Composite> {
    let mut names = Vec::new();
    let mut cover = None;
    attr.parse_nested_meta(|meta| {
        if is_cover(&meta) {
            if cover.is_some() {
                return Err(meta.error("an index takes one cover list"));
            }
            cover = Some(cover_list(&meta, fields)?);
            return Ok(());
        }
        names.push(meta.path.require_ident()?.clone());
        Ok(())
    })?;
    if names.len() < 2 {
        return Err(syn::Error::new(
            attr.span(),
            "a struct level index needs at least two fields",
        ));
    }
    let indexed = names
        .iter()
        .map(|name| field_named(fields, name))
        .collect::<syn::Result<_>>()?;
    Ok((indexed, cover.unwrap_or_default()))
}

fn check_stored(ix: &Indexed, skipped: &[Ident]) -> syn::Result<()> {
    match ix.fields.iter().find(|(ident, _)| skipped.contains(ident)) {
        Some((ident, _)) => Err(syn::Error::new(
            ident.span(),
            format!(
                "`{ident}` is not stored, so it cannot be indexed: rows read back would hold its default while the index kept the values written"
            ),
        )),
        None => Ok(()),
    }
}

fn check_cover(ix: &Indexed, key: &Ident, skipped: &[Ident]) -> syn::Result<()> {
    for (at, (ident, _)) in ix.cover.iter().enumerate() {
        let fail = |message: String| Err(syn::Error::new(ident.span(), message));
        if ix.cover[..at].iter().any(|(other, _)| other == ident) {
            return fail(format!("`{ident}` is covered twice"));
        }
        if ident == key {
            return fail(format!(
                "`{ident}` is the key, which every index entry already holds"
            ));
        }
        if skipped.contains(ident) {
            return fail(format!("`{ident}` is not stored, so it cannot be covered"));
        }
        if ix.indexes(ident).is_some() {
            match ix.kind {
                IndexKind::Value => {}
                IndexKind::Each => {
                    return fail(format!(
                        "an each index holds one element of `{ident}`, so it cannot cover `{ident}`"
                    ));
                }
                IndexKind::Unique => {
                    return fail(format!(
                        "a unique index is looked up by `{ident}`, so it cannot cover `{ident}`"
                    ));
                }
            }
        }
    }
    if ix.stored().len() > MAX_STORED {
        return Err(syn::Error::new(
            ix.span,
            format!("a cover list stores at most {MAX_STORED} fields"),
        ));
    }
    Ok(())
}

fn expand_record(input: DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "Record can only be derived for structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new(
            input.span(),
            "Record requires a struct with named fields",
        ));
    };
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "Record does not support generics",
        ));
    }
    let name = &input.ident;
    let vis: &Visibility = &input.vis;
    let table = format_ident!("{}Table", name);

    let mut key: Option<(Ident, Type)> = None;
    let mut indexes: Vec<Indexed> = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().unwrap();
        for attr in &field.attrs {
            if attr.path().is_ident("key") {
                if key.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "only one #[key] field is allowed",
                    ));
                }
                key = Some((ident.clone(), field.ty.clone()));
            } else if attr.path().is_ident("index") {
                let mut kind = IndexKind::Value;
                let mut cover = Vec::new();
                if matches!(attr.meta, syn::Meta::List(_)) {
                    attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("each") {
                            kind = IndexKind::Each;
                            Ok(())
                        } else if is_cover(&meta) {
                            cover = cover_list(&meta, named)?;
                            Ok(())
                        } else {
                            Err(meta.error("expected `each` or `cover(...)`"))
                        }
                    })?;
                }
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind,
                    cover,
                });
            } else if attr.path().is_ident("unique") {
                let mut cover = Vec::new();
                if matches!(attr.meta, syn::Meta::List(_)) {
                    attr.parse_nested_meta(|meta| {
                        if is_cover(&meta) {
                            cover = cover_list(&meta, named)?;
                            Ok(())
                        } else {
                            Err(meta.error("expected `cover(...)`"))
                        }
                    })?;
                } else {
                    attr.meta.require_path_only()?;
                }
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind: IndexKind::Unique,
                    cover,
                });
            }
        }
    }
    for attr in &input.attrs {
        let kind = if attr.path().is_ident("unique") {
            IndexKind::Unique
        } else if attr.path().is_ident("index") {
            IndexKind::Value
        } else {
            continue;
        };
        let (fields, cover) = composite(attr, named)?;
        let names: Vec<String> = fields.iter().map(|(ident, _)| ident.to_string()).collect();
        indexes.push(Indexed {
            name: names.join("_"),
            span: attr.span(),
            fields,
            kind,
            cover,
        });
    }
    let Some((key_ident, key_ty)) = key else {
        return Err(syn::Error::new(
            input.ident.span(),
            "Record requires exactly one #[key] field",
        ));
    };
    let fields = parse_fields(&data.fields)?;
    let field_of = |ident: &Ident| {
        fields
            .iter()
            .find(|field| field.ident.as_ref() == Some(ident))
    };
    let skipped: Vec<Ident> = fields
        .iter()
        .filter(|field| !field.stored())
        .filter_map(|field| field.ident.clone())
        .collect();
    if skipped.contains(&key_ident) {
        return Err(syn::Error::new(
            key_ident.span(),
            format!(
                "`{key_ident}` is not stored, so it cannot be the key: rows read back would hold its default rather than the key they are stored under"
            ),
        ));
    }
    for ix in &indexes {
        check_stored(ix, &skipped)?;
        check_cover(ix, &key_ident, &skipped)?;
    }

    let stored_name = |ident: &Ident| -> String {
        field_of(ident).map_or_else(|| ident.to_string(), |field| field.name.clone())
    };
    let rows_ty: Type = syn::parse_quote!(::ferro::Map<#key_ty, #name>);
    let map_tys: Vec<Type> = indexes
        .iter()
        .map(|ix| {
            let vt = ix.value_ty();
            let st = ix.stored_ty();
            match ix.kind {
                IndexKind::Unique if ix.stores_cover() => {
                    syn::parse_quote!(::ferro::Map<#vt, (#key_ty, #st)>)
                }
                IndexKind::Unique => syn::parse_quote!(::ferro::Map<#vt, #key_ty>),
                _ => syn::parse_quote!(::ferro::Map<(#vt, #key_ty), #st>),
            }
        })
        .collect();
    let default_of =
        |ident: &Ident| -> Option<TokenStream2> { field_of(ident).and_then(Field::default_value) };
    let fingerprints: Vec<TokenStream2> = indexes
        .iter()
        .map(|ix| ix.fingerprint(&key_ty, &stored_name, &default_of))
        .collect();
    let index_fields: Vec<Ident> = indexes.iter().map(Indexed::field).collect();
    let entry_tys: Vec<TokenStream2> = indexes
        .iter()
        .map(|ix| {
            let vt = ix.value_ty();
            let st = ix.stored_ty();
            match ix.kind {
                IndexKind::Unique if ix.stores_cover() => quote! { (#vt, (#key_ty, #st)) },
                IndexKind::Unique => quote! { (#vt, #key_ty) },
                _ => quote! { ((#vt, #key_ty), #st) },
            }
        })
        .collect();
    let entries_fns: Vec<Ident> = index_fields
        .iter()
        .map(|field| format_ident!("__entries_{}", field))
        .collect();
    let delta_fns: Vec<Ident> = index_fields
        .iter()
        .map(|field| format_ident!("__delta_{}", field))
        .collect();

    let mut reindex = Vec::new();
    let mut try_entries = Vec::new();
    let mut deltas = Vec::new();
    let mut clashes = Vec::new();
    let mut methods = Vec::new();
    let mut finds = Vec::new();
    for (slot, ix) in indexes.iter().enumerate() {
        let field = ix.field();
        let vt = ix.value_ty();
        let st = ix.stored_ty();
        let ct = ix.cover_ty();
        let owned = ix.owned();
        let stored_of = ix.stored_of();
        let same_cover = ix.same_cover();
        let project = ix.project_path();
        let same = ix.fields.iter().map(|(ident, _)| {
            if ix.cover.iter().any(|(covered, _)| covered == ident) {
                quote! { ::ferro::table::same_stored(&__o.#ident, &__n.#ident) }
            } else {
                quote! { __o.#ident == __n.#ident }
            }
        });
        let same = quote! { #(#same &&)* __o.#key_ident == __n.#key_ident };
        let stored_alike = ix
            .stored()
            .into_iter()
            .map(|(ident, _)| quote! { ::ferro::Data::same_encoding(&__o.#ident, &__n.#ident) });
        let stored_alike = quote! { true #(&& #stored_alike)* };
        let indexed_alike = ix
            .fields
            .iter()
            .map(|(ident, _)| quote! { ::ferro::Data::same_encoding(&__o.#ident, &__n.#ident) });
        let each_delta = match (&ix.kind, ix.sole()) {
            (IndexKind::Each, Some((ident, _))) => quote! {
                if #stored_alike {
                    let (__left, __came) = ::ferro::table::IndexEach::delta(&__o.#ident, &__n.#ident, __differ)?;
                    let __r = __o;
                    let __stored = #stored_of;
                    for __v in __left {
                        __gone.push(((__v, ::std::clone::Clone::clone(&__r.#key_ident)), ::std::clone::Clone::clone(&__stored)));
                    }
                    let __r = __n;
                    let __stored = #stored_of;
                    for __v in __came {
                        __come.push(((__v, ::std::clone::Clone::clone(&__r.#key_ident)), ::std::clone::Clone::clone(&__stored)));
                    }
                    return ::std::result::Result::Ok(());
                }
            },
            _ => quote! {},
        };
        let entries_fn = &entries_fns[slot];
        deltas.push(quote! {
            if let (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) = (__old, __new) {
                if #stored_alike #(&& #indexed_alike)* {
                    return ::std::result::Result::Ok(());
                }
                #each_delta
            }
            if let ::std::option::Option::Some(__r) = __old {
                Self::#entries_fn(__r, ::std::option::Option::Some(__differ), __gone)?;
            }
            if let ::std::option::Option::Some(__r) = __new {
                Self::#entries_fn(__r, ::std::option::Option::Some(__differ), __come)?;
            }
            ::std::result::Result::Ok(())
        });
        if !ix.cover.is_empty() {
            let project_fn = ix.project_fn();
            let projected = ix.projected();
            let signature = match ix.kind {
                IndexKind::Unique => quote! { __stored: #st },
                _ => quote! { __value: &#vt, __stored: #st },
            };
            methods.push(quote! {
                fn #project_fn(#signature) -> #ct {
                    #projected
                }
            });
        }
        let values_fn = ix.values_fn();
        let values = ix.values();
        methods.push(quote! {
            fn #values_fn(__r: &#name, __f: &mut dyn ::std::ops::FnMut(&#vt) -> bool) -> bool {
                #values
            }
        });
        match ix.kind {
            IndexKind::Value if ix.stores_cover() => {
                try_entries.push(quote! {
                    __out.push(((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), #stored_of));
                });
                reindex.push(quote! {
                    {
                        let (__same, __same_cover) = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => (#same, #same_cover),
                            _ => (false, false),
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__r) = __old {
                                self.#field.remove(&(#owned, ::std::clone::Clone::clone(&__r.#key_ident)));
                            }
                        }
                        if !__same || !__same_cover {
                            if let ::std::option::Option::Some(__r) = __new {
                                self.#field.insert((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), #stored_of);
                            }
                        }
                    }
                });
            }
            IndexKind::Value => {
                try_entries.push(quote! {
                    __out.push(((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), ()));
                });
                reindex.push(quote! {
                    {
                        let __same = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => #same,
                            _ => false,
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__r) = __old {
                                self.#field.remove(&(#owned, ::std::clone::Clone::clone(&__r.#key_ident)));
                            }
                            if let ::std::option::Option::Some(__r) = __new {
                                self.#field.insert((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), ());
                            }
                        }
                    }
                });
            }
            IndexKind::Unique => {
                let optional = ix.optional();
                let index_name = &ix.name;
                let (entry, held, entry_ty) = if ix.stores_cover() {
                    (
                        quote! { (::std::clone::Clone::clone(&__r.#key_ident), #stored_of) },
                        quote! { &__holder.0 },
                        quote! { (#key_ty, #st) },
                    )
                } else {
                    (
                        quote! { ::std::clone::Clone::clone(&__r.#key_ident) },
                        quote! { &*__holder },
                        quote! { #key_ty },
                    )
                };
                try_entries.push(quote! {
                    if let ::std::option::Option::Some(__value) = #optional {
                        __out.push((__value, #entry));
                    }
                });
                reindex.push(quote! {
                    {
                        let (__same, __same_cover) = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => (#same, #same_cover),
                            _ => (false, false),
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__value) = __old.and_then(|__r| #optional) {
                                self.#field.remove(&__value);
                            }
                        }
                        if !__same || !__same_cover {
                            if let ::std::option::Option::Some(__r) = __new {
                                if let ::std::option::Option::Some(__value) = #optional {
                                    self.#field.insert(__value, #entry);
                                }
                            }
                        }
                    }
                });
                clashes.push((slot, quote! {
                    {
                        let __r = __row;
                        if let ::std::option::Option::Some(__value) = #optional {
                            if let ::std::option::Option::Some(__holder) = self.#field.get(&__value) {
                                let __held: &#key_ty = #held;
                                if *__held != __r.#key_ident && ::std::option::Option::Some(__held) != __moving {
                                    return ::std::option::Option::Some(::ferro::table::Taken {
                                        index: #index_name,
                                        holder: ::std::clone::Clone::clone(__held),
                                    });
                                }
                            }
                        }
                    }
                }));
                let by = ix.method();
                let key_by = format_ident!("key_{}", by);
                let subject = ix.subject();
                let matching = ix.matching();
                let held_doc = if ix.stores_cover() {
                    "the key and stored covered fields"
                } else {
                    "the key"
                };
                let index_doc = format!(
                    "The unique index on {subject}, from each value to {held_doc} of the row that holds it."
                );
                let key_by_doc =
                    format!("The key of the row {matching}, read from the index alone.");
                let by_doc = format!("The row {matching}.");
                methods.push(quote! {
                    #[doc = #index_doc]
                    pub fn #field(&self) -> &::ferro::Map<#vt, #entry_ty> {
                        &self.#field
                    }

                    #[doc = #key_by_doc]
                    pub fn #key_by<__Q>(&self, value: &__Q) -> ::std::option::Option<#key_ty>
                    where
                        #vt: ::std::borrow::Borrow<__Q>,
                        __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    {
                        self.#field.get(value).map(|__holder| {
                            let __held: &#key_ty = #held;
                            ::std::clone::Clone::clone(__held)
                        })
                    }
                });
                if ix.stores_cover() {
                    let covered_by = format_ident!("covered_{}", by);
                    let project_fn = ix.project_fn();
                    let covered_doc = format!(
                        "The key and covered fields of the row {matching}, read from the index alone."
                    );
                    methods.push(quote! {
                        #[doc = #by_doc]
                        pub fn #by<__Q>(&self, value: &__Q) -> ::std::option::Option<::ferro::Val<#name>>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            let __holder = self.#field.get(value)?;
                            self.rows.get::<#key_ty>(&__holder.0)
                        }

                        #[doc = #covered_doc]
                        pub fn #covered_by<__Q>(&self, value: &__Q) -> ::std::option::Option<(#key_ty, #ct)>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            self.#field.get(value).map(|__holder| {
                                let (__key, __stored) = ::ferro::Val::into_owned(__holder);
                                (__key, Self::#project_fn(__stored))
                            })
                        }
                    });
                } else {
                    methods.push(quote! {
                        #[doc = #by_doc]
                        pub fn #by<__Q>(&self, value: &__Q) -> ::std::option::Option<::ferro::Val<#name>>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            ::ferro::table::unique_eq::<#vt, #name, __Q>(&self.rows, &self.#field, value)
                        }
                    });
                }
                let rows_of = |entries: TokenStream2| {
                    if ix.stores_cover() {
                        let project_fn = ix.project_fn();
                        quote! {
                            ::ferro::table::unique_rows_covered::<#vt, #name, #st, #ct>(&self.rows, #entries, Self::#project_fn, Self::#values_fn)
                        }
                    } else {
                        quote! {
                            ::ferro::table::unique_rows::<#vt, #name>(&self.rows, #entries, Self::#values_fn)
                        }
                    }
                };
                let covers = ix.covers();
                let range = format_ident!("{}_range", by);
                let range_doc = if ix.sparse().is_some() {
                    format!(
                        "The rows whose {subject} is `Some(value)` for a `value` in `range`, ordered by that value. A row whose {subject} is `None` is not indexed.{covers}"
                    )
                } else {
                    format!(
                        "The rows whose {subject} lies in `range`, ordered by {subject}.{covers}"
                    )
                };
                let range_rows = rows_of(quote! { self.#field.range::<__Q, __R>(range) });
                methods.push(quote! {
                    #[doc = #range_doc]
                    #[inline]
                    pub fn #range<__Q, __R>(&self, range: __R) -> ::ferro::table::IndexIter<#name, #ct>
                    where
                        #vt: ::std::borrow::Borrow<__Q>,
                        __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        __R: ::std::ops::RangeBounds<__Q>,
                    {
                        #range_rows
                    }
                });
                if ix.sole().is_none() {
                    let prefix = format_ident!("{}_prefix", by);
                    let (head, first) = &ix.fields[0];
                    let rest = ix.rest();
                    let prefix_doc =
                        format!("The rows whose `{head}` is `value`, ordered by {rest}.{covers}");
                    let prefix_rows = rows_of(quote! {
                        self.#field.range_by(
                            |__value| ::std::borrow::Borrow::<__Q>::borrow(&__value.0) < value,
                            |__value| ::std::borrow::Borrow::<__Q>::borrow(&__value.0) <= value,
                        )
                    });
                    methods.push(quote! {
                        #[doc = #prefix_doc]
                        #[inline]
                        pub fn #prefix<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
                        where
                            #first: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            #prefix_rows
                        }
                    });
                }
                continue;
            }
            IndexKind::Each if ix.stores_cover() => {
                let ident = &ix.fields[0].0;
                try_entries.push(quote! {
                    let __stored = #stored_of;
                    for __v in ::ferro::table::each_values(&__r.#ident, __differ)? {
                        __out.push(((__v, ::std::clone::Clone::clone(&__r.#key_ident)), ::std::clone::Clone::clone(&__stored)));
                    }
                });
                reindex.push(quote! {
                    {
                        let __collect = |__r: ::std::option::Option<&#name>| -> ::std::collections::BTreeSet<(#vt, #key_ty)> {
                            match __r {
                                ::std::option::Option::Some(__r) => ::ferro::table::IndexEach::values(&__r.#ident)
                                    .into_iter()
                                    .map(|__v| (__v, ::std::clone::Clone::clone(&__r.#key_ident)))
                                    .collect(),
                                ::std::option::Option::None => ::std::collections::BTreeSet::new(),
                            }
                        };
                        let __same_cover = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => #same_cover,
                            _ => false,
                        };
                        let __before = __collect(__old);
                        let __after = __collect(__new);
                        for __entry in __before.difference(&__after) {
                            self.#field.remove(__entry);
                        }
                        if let ::std::option::Option::Some(__r) = __new {
                            let __stored = #stored_of;
                            for __entry in &__after {
                                if !__same_cover || !__before.contains(__entry) {
                                    self.#field.insert(::std::clone::Clone::clone(__entry), ::std::clone::Clone::clone(&__stored));
                                }
                            }
                        }
                    }
                });
            }
            IndexKind::Each => {
                let ident = &ix.fields[0].0;
                try_entries.push(quote! {
                    for __v in ::ferro::table::each_values(&__r.#ident, __differ)? {
                        __out.push(((__v, ::std::clone::Clone::clone(&__r.#key_ident)), ()));
                    }
                });
                reindex.push(quote! {
                    {
                        let __collect = |__r: ::std::option::Option<&#name>| -> ::std::collections::BTreeSet<(#vt, #key_ty)> {
                            match __r {
                                ::std::option::Option::Some(__r) => ::ferro::table::IndexEach::values(&__r.#ident)
                                    .into_iter()
                                    .map(|__v| (__v, ::std::clone::Clone::clone(&__r.#key_ident)))
                                    .collect(),
                                ::std::option::Option::None => ::std::collections::BTreeSet::new(),
                            }
                        };
                        let __old = __collect(__old);
                        let __new = __collect(__new);
                        for __entry in __old.difference(&__new) {
                            self.#field.remove(__entry);
                        }
                        for __entry in __new.difference(&__old) {
                            self.#field.insert(::std::clone::Clone::clone(__entry), ());
                        }
                    }
                });
            }
        }
        let range = format_ident!("{}_range", field);
        let counts = format_ident!("{}_counts", field);
        let after = format_ident!("{}_after", field);
        let subject = ix.subject();
        let matching = ix.matching();
        let covers = ix.covers();
        let combined = |method: &Ident| {
            format!(
                "[`{table}::{method}`] as [`Matches`](ferro::table::Matches), to combine with `and` and `or`."
            )
        };
        let (range_doc, counts_doc) = match ix.kind {
            IndexKind::Each => (
                format!(
                    "The rows with an element of {subject} in `range`, once for each such element, ordered by element and then by key.{covers}"
                ),
                format!(
                    "Each distinct element of {subject} with the number of rows that hold it, in order, read from the index alone."
                ),
            ),
            _ => (
                format!(
                    "The rows whose {subject} lies in `range`, ordered by {subject} and then by key.{covers}"
                ),
                format!(
                    "Each distinct {subject} with the number of rows that hold it, in order, read from the index alone."
                ),
            ),
        };
        let by_doc = format!("The rows {matching}, in key order.{covers}");
        let after_doc = format!(
            "The rows {matching} and whose key is greater than `after`, in key order: the page after the row with key `after`.{covers}"
        );
        if let (IndexKind::Value, None) = (&ix.kind, ix.sole()) {
            let prefix = format_ident!("{}_prefix", field);
            let (head, first) = &ix.fields[0];
            let rest = ix.rest();
            let prefix_doc = format!(
                "The rows whose `{head}` is `value`, ordered by {rest} and then by key.{covers}"
            );
            methods.push(quote! {
                #[doc = #prefix_doc]
                #[inline]
                pub fn #prefix<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #first: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_where::<#vt, #name, #st, #ct>(
                        &self.rows,
                        &self.#field,
                        #project,
                        |__entry| ::std::borrow::Borrow::<__Q>::borrow(&__entry.0) < value,
                        |__entry| ::std::borrow::Borrow::<__Q>::borrow(&__entry.0) <= value,
                        Self::#values_fn,
                    )
                }
            });
        }
        let wrapped = ix.sole().and_then(|(_, ty)| option_inner(ty));
        if let (IndexKind::Value, Some(inner)) = (&ix.kind, wrapped) {
            let some = format_ident!("{}_some", field);
            let some_after = format_ident!("{}_some_after", field);
            let some_doc =
                format!("The rows whose {subject} is `Some(value)`, in key order.{covers}");
            let some_after_doc = format!(
                "The rows whose {subject} is `Some(value)` and whose key is greater than `after`, in key order.{covers}"
            );
            let some_find_doc = combined(&some);
            let some_after_find_doc = combined(&some_after);
            methods.push(quote! {
                #[doc = #some_doc]
                #[inline]
                pub fn #some<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_eq_some::<#inner, #name, #st, #ct, __Q>(&self.rows, &self.#field, #project, value)
                }

                #[doc = #some_after_doc]
                #[inline]
                pub fn #some_after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    #key_ty: ::std::borrow::Borrow<__A>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    __A: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_after_some::<#inner, #name, #st, #ct, __Q, __A>(&self.rows, &self.#field, #project, value, after)
                }
            });
            let found = ix.found_path(&table);
            finds.push(quote! {
                #[doc = #some_find_doc]
                #[inline]
                pub fn #some<__Q>(&self, value: &__Q) -> ::ferro::table::Matches<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::match_eq_some::<#inner, #name, #st, #ct, __Q>(&self.table.rows, &self.table.#field, #found, value)
                }

                #[doc = #some_after_find_doc]
                #[inline]
                pub fn #some_after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::Matches<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    #key_ty: ::std::borrow::Borrow<__A>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    __A: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::match_after_some::<#inner, #name, #st, #ct, __Q, __A>(&self.table.rows, &self.table.#field, #found, value, after)
                }
            });
        }
        let found = ix.found_path(&table);
        let by_find_doc = combined(&field);
        let after_find_doc = combined(&after);
        finds.push(quote! {
            #[doc = #by_find_doc]
            #[inline]
            pub fn #field<__Q>(&self, value: &__Q) -> ::ferro::table::Matches<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::match_eq::<#vt, #name, #st, #ct, __Q>(&self.table.rows, &self.table.#field, #found, value)
            }

            #[doc = #after_find_doc]
            #[inline]
            pub fn #after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::Matches<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                #key_ty: ::std::borrow::Borrow<__A>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __A: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::match_after::<#vt, #name, #st, #ct, __Q, __A>(&self.table.rows, &self.table.#field, #found, value, after)
            }
        });
        methods.push(quote! {
            #[doc = #counts_doc]
            pub fn #counts(&self) -> ::std::vec::Vec<(#vt, usize)> {
                ::ferro::table::index_counts(&self.#field)
            }

            #[doc = #by_doc]
            #[inline]
            pub fn #field<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::index_eq::<#vt, #name, #st, #ct, __Q>(&self.rows, &self.#field, #project, value)
            }

            #[doc = #after_doc]
            #[inline]
            pub fn #after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                #key_ty: ::std::borrow::Borrow<__A>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __A: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::index_after::<#vt, #name, #st, #ct, __Q, __A>(&self.rows, &self.#field, #project, value, after)
            }

            #[doc = #range_doc]
            #[inline]
            pub fn #range<__Q, __R>(&self, range: __R) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __R: ::std::ops::RangeBounds<__Q>,
            {
                ::ferro::table::index_range::<#vt, #name, #st, #ct, __Q, __R>(&self.rows, &self.#field, #project, range, Self::#values_fn)
            }
        });
    }

    let find = format_ident!("{}Find", name);
    let find_doc = format!(
        "The equality lookups of [`{table}`], returning [`Matches`](ferro::table::Matches) that `and` and `or` combine by primary key. [`{table}::find`] makes one."
    );
    let find_fn_doc = "A view of the equality lookups of every non-unique index, returning [`Matches`](ferro::table::Matches) that `and` and `or` combine by primary key, reading a row only for a key that matches or that is tested against a range or prefix lookup.";
    let find_items = if finds.is_empty() {
        quote! {}
    } else {
        quote! {
            #[doc = #find_doc]
            #vis struct #find<'a> {
                table: &'a #table,
            }

            impl ::std::clone::Clone for #find<'_> {
                fn clone(&self) -> Self {
                    *self
                }
            }

            impl ::std::marker::Copy for #find<'_> {}

            impl #table {
                #[doc = #find_fn_doc]
                pub fn find(&self) -> #find<'_> {
                    #find { table: self }
                }
            }

            impl #find<'_> {
                #(#finds)*
            }
        }
    };

    let rows_tag = field_tag("rows");
    let index_tag = field_tag("indexes");
    let count = indexes.len();
    let total = count as u64 + 1;
    let slots: Vec<usize> = (0..count).collect();
    let vars: Vec<Ident> = (0..count).map(|i| format_ident!("__index{}", i)).collect();
    let started: Vec<Ident> = (0..count)
        .map(|i| format_ident!("__started{}", i))
        .collect();
    let each_tys: Vec<&Type> = indexes
        .iter()
        .filter(|ix| matches!(ix.kind, IndexKind::Each))
        .map(|ix| &ix.fields[0].1)
        .collect();
    let reindex_fns: Vec<Ident> = index_fields
        .iter()
        .map(|field| format_ident!("__reindex_{}", field))
        .collect();
    let clash_slots: Vec<usize> = clashes.iter().map(|(slot, _)| *slot).collect();
    let clash_fns: Vec<Ident> = clash_slots
        .iter()
        .map(|slot| format_ident!("__clash_{}", index_fields[*slot]))
        .collect();
    let clash_blocks = clashes.iter().map(|(_, block)| block);

    let encode_rows = quote! {
        __w.field(#rows_tag, |__w| ::ferro::Data::encode(&self.rows, __w))?;
    };
    let encode_indexes = if count == 0 {
        quote! {}
    } else {
        quote! {
            let __fingerprints = Self::__fingerprints();
            for &__slot in __fingerprints.order() {
                let __fingerprint = __fingerprints.get(__slot);
                match __slot {
                    #(#slots => __w.field(#index_tag, |__w| ::ferro::table::encode_index(__fingerprint, &self.#index_fields, __w))?,)*
                    _ => {}
                }
            }
        }
    };
    let (encode_first, encode_last) = if rows_tag < index_tag {
        (encode_rows, encode_indexes)
    } else {
        (encode_indexes, encode_rows)
    };
    let fingerprints_of = if count == 0 {
        quote! {}
    } else {
        quote! { let __fingerprints = Self::__fingerprints(); }
    };
    let decode_arm = if count == 0 {
        quote! {}
    } else {
        quote! {
            #index_tag => match __fingerprints.slot(__r) {
                #(::std::option::Option::Some(#slots) => #vars = ::std::option::Option::Some(::ferro::Data::decode(__r)?),)*
                _ => __unknown = true,
            },
        }
    };
    let visit_arm = if count == 0 {
        quote! {}
    } else {
        quote! {
            #index_tag => match __fingerprints.slot(__r) {
                #(::std::option::Option::Some(#slots) => <#map_tys as ::ferro::Data>::visit(__r)?,)*
                _ => {}
            },
        }
    };
    let rebuild = if count == 0 {
        quote! {}
    } else {
        quote! {
            if __stale.contains(&true) && __r.rebuilds() {
                let __rows = ::ferro::table::stored_rows::<#name>(&__table.rows)?;
                for __row in __rows {
                    #(
                        if __stale[#clash_slots] {
                            if let ::std::option::Option::Some(__taken) = __table.#clash_fns(&*__row, ::std::option::Option::None) {
                                return ::std::result::Result::Err(::ferro::Error::Decode(::std::format!(
                                    "two rows of {} share a value of unique index {}",
                                    ::std::stringify!(#name),
                                    __taken.index,
                                )));
                            }
                        }
                    )*
                    #(
                        if __stale[#slots] {
                            let mut __entries = ::std::vec::Vec::new();
                            Self::#entries_fns(&*__row, ::std::option::Option::None, &mut __entries)?;
                            for (__key, __value) in __entries {
                                __table.#index_fields.insert(__key, __value);
                            }
                        }
                    )*
                }
            }
        }
    };
    let fingerprints_fn = if count == 0 {
        quote! {}
    } else {
        quote! {
            fn __fingerprints() -> &'static ::ferro::table::Fingerprints<#count> {
                static __FINGERPRINTS: ::std::sync::OnceLock<::ferro::table::Fingerprints<#count>> =
                    ::std::sync::OnceLock::new();
                __FINGERPRINTS.get_or_init(|| {
                    ::ferro::table::Fingerprints::new(::std::stringify!(#name), [#(#fingerprints),*])
                })
            }
        }
    };
    let unknown_of = if count == 0 {
        quote! {}
    } else {
        quote! { let mut __unknown = false; }
    };
    let read_fields = quote! {
        #fingerprints_of
        #unknown_of
        let mut __rows: ::std::option::Option<#rows_ty> = ::std::option::Option::None;
        #(let mut #vars: ::std::option::Option<#map_tys> = ::std::option::Option::None;)*
        __r.fields(|__tag, __r| {
            match __tag {
                #rows_tag => __rows = ::std::option::Option::Some(::ferro::Data::decode(__r)?),
                #decode_arm
                _ => {}
            }
            ::std::result::Result::Ok(())
        })?;
    };
    let visit_fields = quote! {
        #fingerprints_of
        __r.fields(|__tag, __r| {
            match __tag {
                #rows_tag => <#rows_ty as ::ferro::Data>::visit(__r)?,
                #visit_arm
                _ => {}
            }
            ::std::result::Result::Ok(())
        })
    };
    let index_names: Vec<&str> = indexes.iter().map(|ix| ix.name.as_str()).collect();
    let (decode_fn, visit_fn, stored_fn, slot_fns) = if count == 0 {
        (
            quote! {
                fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                    #read_fields
                    ::std::result::Result::Ok(#table {
                        rows: ::ferro::codec::required(__rows, "rows")?,
                    })
                }
            },
            quote! {
                fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                    #visit_fields
                }
            },
            quote! {},
            quote! {},
        )
    } else {
        (
            quote! {
                fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                    let ((mut __table, mut __stale, _), __unchecked) =
                        ::ferro::table::noted::<#name, _>(__r, Self::__stored)?;
                    if __unchecked {
                        __stale = [true; #count];
                        #(__table.#index_fields = ::std::default::Default::default();)*
                    }
                    #rebuild
                    ::std::result::Result::Ok(__table)
                }
            },
            quote! {
                fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                    ::ferro::table::noted::<#name, _>(__r, |__r| {
                        #visit_fields
                    })
                    .map(|_| ())
                }
            },
            quote! {
                fn __stored(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<(Self, [bool; #count], bool)> {
                    #read_fields
                    let __stale = [#(#vars.is_none()),*];
                    let __table = #table {
                        rows: ::ferro::codec::required(__rows, "rows")?,
                        #(#index_fields: ::std::option::Option::unwrap_or_default(#vars)),*
                    };
                    ::std::result::Result::Ok((__table, __stale, __unknown))
                }
            },
            quote! {
                fn decode_stored(
                    __r: &mut ::ferro::codec::Decoder<'_>,
                ) -> ::ferro::Result<::ferro::table::Stored<Self>> {
                    let (__table, __stale, __unknown) = Self::__stored(__r)?;
                    ::std::result::Result::Ok(::ferro::table::Stored {
                        table: __table,
                        held: __stale.iter().map(|__stale| !*__stale).collect(),
                        unknown: __unknown,
                    })
                }

                fn indexing_reads() -> bool {
                    false #(|| <#each_tys as ::ferro::table::IndexEach>::READS)*
                }

                fn diff_slots(
                    &self,
                    __later: &Self,
                    __slots: &[bool],
                    __differ: &::ferro::table::Differ<'_, '_>,
                    __rows: &mut dyn ::std::ops::FnMut() -> ::ferro::Result<::std::vec::Vec<::ferro::table::RowChange<#name>>>,
                ) -> ::ferro::Result<::std::option::Option<(&'static str, ::ferro::table::Fault)>> {
                    #(
                        let #started = (__slots.get(#slots) == ::std::option::Option::Some(&true))
                            .then(|| __differ.start(&self.#index_fields, &__later.#index_fields));
                    )*
                    let __moved = __rows()?;
                    #(
                        if let ::std::option::Option::Some(__started) = #started {
                            let mut __gone = ::std::vec::Vec::new();
                            let mut __come = ::std::vec::Vec::new();
                            for (__old, __new) in &__moved {
                                Self::#delta_fns(
                                    __old.as_ref(),
                                    __new.as_ref(),
                                    __differ,
                                    &mut __gone,
                                    &mut __come,
                                )?;
                            }
                            if let ::std::option::Option::Some(__fault) = __differ.index(__started, __gone, __come)? {
                                return ::std::result::Result::Ok(::std::option::Option::Some((#index_names, __fault)));
                            }
                        }
                    )*
                    ::std::result::Result::Ok(::std::option::Option::None)
                }

                fn built_slots(
                    &self,
                    __slots: &[bool],
                    __differ: &::ferro::table::Differ<'_, '_>,
                    __rows: &mut ::ferro::table::EveryRow<'_, #name>,
                ) -> ::ferro::Result<::std::option::Option<(&'static str, ::ferro::table::Fault)>> {
                    #(
                        let mut #vars: ::std::option::Option<::std::vec::Vec<#entry_tys>> =
                            (__slots.get(#slots) == ::std::option::Option::Some(&true)).then(::std::vec::Vec::new);
                    )*
                    __rows(&mut |__r| {
                        #(
                            if let ::std::option::Option::Some(__out) = &mut #vars {
                                Self::#entries_fns(__r, ::std::option::Option::Some(__differ), __out)?;
                            }
                        )*
                        ::std::result::Result::Ok(())
                    })?;
                    #(
                        let #started = #vars.map(|__entries| __differ.verify(&self.#index_fields, __entries));
                    )*
                    #(
                        if let ::std::option::Option::Some(__verifying) = #started {
                            if let ::std::option::Option::Some(__fault) = __differ.verified(__verifying)? {
                                return ::std::result::Result::Ok(::std::option::Option::Some((#index_names, __fault)));
                            }
                        }
                    )*
                    ::std::result::Result::Ok(::std::option::Option::None)
                }
            },
        )
    };
    let table_doc = format!(
        "The rows of [`{name}`] and the indexes its `Record` derive declares. [`ferro::Table<{name}>`](ferro::Table) derefs to it, so its lookups are called on the table."
    );

    Ok(quote! {
        #[doc = #table_doc]
        #vis struct #table {
            rows: #rows_ty,
            #(#index_fields: #map_tys),*
        }

        impl ::std::clone::Clone for #table {
            fn clone(&self) -> Self {
                #table {
                    rows: ::std::clone::Clone::clone(&self.rows),
                    #(#index_fields: ::std::clone::Clone::clone(&self.#index_fields)),*
                }
            }
        }

        impl ::std::default::Default for #table {
            fn default() -> Self {
                #table {
                    rows: ::std::default::Default::default(),
                    #(#index_fields: ::std::default::Default::default()),*
                }
            }
        }

        impl ::ferro::Data for #table {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                __w.varint(#total);
                #encode_first
                #encode_last
                ::std::result::Result::Ok(())
            }

            #decode_fn

            #visit_fn

            fn same_encoding(&self, __other: &Self) -> bool {
                ::ferro::Data::same_encoding(&self.rows, &__other.rows)
                    #(&& ::ferro::Data::same_encoding(&self.#index_fields, &__other.#index_fields))*
            }
        }

        impl ::ferro::Record for #name {
            type Key = #key_ty;
            type Table = #table;

            fn key(&self) -> #key_ty {
                ::std::clone::Clone::clone(&self.#key_ident)
            }
        }

        impl ::ferro::TableData<#name> for #table {
            fn rows(&self) -> &::ferro::Map<#key_ty, #name> {
                &self.rows
            }

            fn rows_mut(&mut self) -> &mut ::ferro::Map<#key_ty, #name> {
                &mut self.rows
            }

            fn reindex(&mut self, __old: ::std::option::Option<&#name>, __new: ::std::option::Option<&#name>) {
                #(self.#reindex_fns(__old, __new);)*
            }

            fn conflict(
                &self,
                __row: &#name,
                __moving: ::std::option::Option<&#key_ty>,
            ) -> ::std::option::Option<::ferro::table::Taken<#key_ty>> {
                #(
                    if let ::std::option::Option::Some(__taken) = self.#clash_fns(__row, __moving) {
                        return ::std::option::Option::Some(__taken);
                    }
                )*
                ::std::option::Option::None
            }

            #slot_fns
        }

        impl #table {
            #fingerprints_fn

            #stored_fn

            #(
                #[inline]
                fn #reindex_fns(&mut self, __old: ::std::option::Option<&#name>, __new: ::std::option::Option<&#name>) {
                    #reindex
                }
            )*

            #(
                fn #entries_fns(
                    __r: &#name,
                    __differ: ::std::option::Option<&::ferro::table::Differ<'_, '_>>,
                    __out: &mut ::std::vec::Vec<#entry_tys>,
                ) -> ::ferro::Result<()> {
                    #try_entries
                    ::std::result::Result::Ok(())
                }

                fn #delta_fns(
                    __old: ::std::option::Option<&#name>,
                    __new: ::std::option::Option<&#name>,
                    __differ: &::ferro::table::Differ<'_, '_>,
                    __gone: &mut ::std::vec::Vec<#entry_tys>,
                    __come: &mut ::std::vec::Vec<#entry_tys>,
                ) -> ::ferro::Result<()> {
                    #deltas
                }
            )*

            #(
                #[inline]
                fn #clash_fns(
                    &self,
                    __row: &#name,
                    __moving: ::std::option::Option<&#key_ty>,
                ) -> ::std::option::Option<::ferro::table::Taken<#key_ty>> {
                    #clash_blocks
                    ::std::option::Option::None
                }
            )*

            #(#methods)*
        }

        #find_items
    })
}

/// Derives `ferro::Record` for a struct with named fields, making it a row of `ferro::Table` with
/// the secondary indexes its attributes declare.
///
/// Use it through the re-export, `use ferro::{Data, Record, Table};`. The struct must also derive
/// `Data` and `Clone`, and cannot have generic parameters. Exactly one field is marked `#[key]`,
/// the primary key, and its type and every indexed type must be `Data + Ord`.
///
/// The derive generates a `{Name}Table` struct, with the struct's visibility, that holds the rows
/// and one map per index, and `Table<{Name}>` derefs to it, so the lookups below are called on the
/// table. Every write through `Table` keeps every index in step with the rows. With at least one
/// non-unique index it also generates `{Name}Find`, the view `find()` returns.
///
/// [The tables guide](https://github.com/archlemon/ferro/blob/main/docs/guide/tables.md) describes
/// tables and indexes at more length, and the docs of `ferro::table` cover `IndexIter`, `Matches`
/// and the rest of what the lookups return.
///
/// # Attributes
///
/// On fields:
///
/// | Attribute | Declares |
/// | --- | --- |
/// | `#[key]` | The primary key. Exactly one field has it, and it cannot be `#[ferro(skip)]`. |
/// | `#[index]` | An index on the field's value. |
/// | `#[index(each)]` | An index on every element of a `Vec<X>`, `BTreeSet<X>`, `Set<X>` or `Option<X>` field. |
/// | `#[index(cover(a, b))]` | An index that also stores fields `a` and `b` beside each entry. Combines with `each`, as `#[index(each, cover(a))]`. |
/// | `#[unique]` | A unique index: at most one row holds each value. On an `Option<X>` field `None` is not indexed, so any number of rows can leave it empty. |
/// | `#[unique(cover(a, b))]` | A unique index that also stores `a` and `b` beside each key. |
///
/// On the struct, for an index on two or more fields in the order named:
///
/// | Attribute | Declares |
/// | --- | --- |
/// | `#[index(a, b)]` | A composite index on `(a, b)`, named `a_b` in its lookups. Takes a cover list too, as `#[index(a, b, cover(c))]`. |
/// | `#[unique(a, b)]` | A unique composite index: at most one row holds each `(a, b)`. Takes a cover list too. |
///
/// `#[ferro(rename = "...")]`, `#[ferro(default)]`, `#[ferro(default = "...")]`,
/// `#[ferro(skip)]` and, on the struct, `#[ferro(removed(...))]` mean what they do for `Data`.
/// Each index is stored with a fingerprint of its kind, the key's type, and the stored names, types
/// and defaults of the fields it is keyed on and stores, where a type's description holds the
/// defaults of the derived structs and enums it reaches. When a table is read, an index whose
/// fingerprint no longer matches the struct is rebuilt from the rows, so changing an index's
/// attribute, its cover list, or the stored name, type or default of one of its fields, or a default
/// nested in its type, rebuilds that index. Turning an `#[index]` into a `#[unique]` fails to open
/// with `Error::Decode` if two rows share a value.
/// [The schema evolution guide](https://github.com/archlemon/ferro/blob/main/docs/guide/schema-evolution.md)
/// lists what rebuilds and what does not.
///
/// # Lookups
///
/// Lookups take the value by reference, borrowed as the type borrows: `by_customer("wile")` for a
/// `String` field. `V` below is the indexed type, the element type for an each index and the
/// tuple of the fields for a composite. `C` holds the covered fields, `()` without a cover list.
///
/// For `#[index] x`, `#[index(each)] x` and a composite `#[index(a, b)]` named `x`:
///
/// | Method | Returns |
/// | --- | --- |
/// | `by_x(&value)` | `IndexIter<Name, C>` of the rows whose `x` is `value`, in key order. |
/// | `by_x_after(&value, &key)` | The same rows with keys greater than `key`, so a page after the row `key` costs a seek. |
/// | `by_x_range(range)` | `IndexIter<Name, C>` of the rows whose `x` lies in `range`, by value and then by key. |
/// | `by_x_counts()` | `Vec<(V, usize)>`: each distinct value with the number of rows that hold it, in value order, from the index alone. |
///
/// An each index has an entry for each distinct element of a row, so its range yields a row once
/// for every element in the range. An index on a field whose type is written `Option<X>` takes
/// `&Option<X>` in `by_x`, and also has:
///
/// | Method | Returns |
/// | --- | --- |
/// | `by_x_some(&value)` | The rows whose `x` is `Some(value)`, where `value` borrows as `X` does. |
/// | `by_x_some_after(&value, &key)` | The same rows with keys greater than `key`. |
///
/// A composite `#[index(a, b, ...)]` also has `by_a_b_prefix(&a)`, the rows whose first field is
/// `a`, ordered by the fields after it and then by key, where `a` borrows as its type does.
///
/// For `#[unique] x`, or a composite `#[unique(a, b)]` named `x`:
///
/// | Method | Returns |
/// | --- | --- |
/// | `by_x(&value)` | `Option<Val<Name>>`, the row that holds `value`. |
/// | `key_by_x(&value)` | `Option<Key>`, that row's key, from the index alone. |
/// | `by_x_range(range)` | `IndexIter<Name, C>` of the rows whose `x` lies in `range`, in value order. |
/// | `unique_x()` | The whole index: `&Map<V, Key>`, or `&Map<V, (Key, S)>` with a cover list, where `S` is the tuple of the stored fields. |
/// | `covered_by_x(&value)` | `Option<(Key, C)>`, the key and covered fields, from the index alone. Only with a cover list. |
///
/// A composite `#[unique(a, b, ...)]` also has `by_a_b_prefix(&a)`, the rows whose first field is
/// `a`, ordered by the fields after it. `by_x_range` and `by_a_b_prefix` return the same
/// `IndexIter` as the lookups of a non-unique index, so `.keys()` and `.covered()` read the index
/// alone, and the iterator runs from either end.
///
/// On an `Option<X>` field a unique index is keyed by `X`, so `by_x` takes `&X` and `by_x_range` a
/// range of `X`, and a row whose `x` is `None` is in no lookup. A clash on insert or update is
/// reported as `Taken { index, holder }`, where `index` is the index's name.
///
/// # Combining lookups
///
/// `find()` on the table returns a `{Name}Find<'_>`, which is `Copy`, with the equality lookups of
/// every non-unique index: `by_x(&value)`, `by_x_after(&value, &key)` and, for an `Option` field,
/// `by_x_some` and `by_x_some_after`. They return `Matches<Name, C>`, which `and` and `or` combine
/// by primary key, reading a row only for a key that matches. `and` and `or` also take any
/// `IndexIter`. A range or prefix lookup of a unique or non-unique index is tested key by key when
/// the other side of an `and` has at most a quarter of its keys, by reading each such key's row,
/// and its keys are read in full and sorted first otherwise. `.keys()` reads no rows but those,
/// and `.covered()` gives the covered fields of the leftmost lookup.
///
/// # Covering indexes
///
/// A cover list stores the fields it names beside every entry of the index, so `.covered()` on a
/// lookup returns `(key, fields)` without reading the rows. One covered field comes back as itself
/// and several as a tuple in the order listed. A covered field is copied into every entry and
/// rewritten when it changes, so cover what a hot page shows and what seldom changes. A field
/// counts as changed when `==` fails or it encodes to other bytes, including a field the index is
/// keyed on that its cover list names, whose entry is then moved.
///
/// A cover list names at least one field, and no field twice. It may name the fields a value or
/// composite index is keyed on, which come from the entry and are not stored twice. It cannot name
/// the key, which every entry holds already, a `#[ferro(skip)]` field, the field an each index
/// indexes, or a field a unique index is keyed on, and it stores at most 12 fields.
///
/// # Compile errors
///
/// The derive refuses, with an error at the attribute or field at fault: anything but a struct
/// with named fields, generic parameters, no `#[key]` field or two, a struct-level index of fewer
/// than two fields, a field name that does not exist, an index option other than `each` and
/// `cover(...)` (only `cover(...)` on `#[unique]`), two cover lists on one struct-level index, a
/// cover list that breaks the rules above, an index of any kind on a `#[ferro(skip)]` field,
/// alone or as part of a composite, and `#[key]` on a `#[ferro(skip)]` field. A skipped field reads
/// back as its default, so the index would keep values that the rows no longer hold:
///
/// ```compile_fail
/// use ferro::{Data, Record};
///
/// #[derive(Data, Record, Clone)]
/// struct Order {
///     #[key]
///     id: u64,
///     #[index]
///     #[ferro(skip)]
///     cached_total: u64,
/// }
/// ```
///
/// and a row read back would hold the default key rather than the one it is stored under, so
/// writing it back would move it there:
///
/// ```compile_fail
/// use ferro::{Data, Record};
///
/// #[derive(Data, Record, Clone)]
/// struct Session {
///     #[key]
///     #[ferro(skip)]
///     token: u64,
///     user: String,
/// }
/// ```
///
/// # Examples
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// enum Status {
///     Open,
///     Closed,
/// }
///
/// #[derive(Data, Record, Clone, Debug, PartialEq)]
/// #[index(project, status)]
/// struct Issue {
///     #[key]
///     id: u64,
///     #[unique]
///     slug: String,
///     #[index]
///     project: String,
///     #[index]
///     status: Status,
///     #[index]
///     assignee: Option<String>,
///     #[index(each)]
///     labels: Vec<String>,
/// }
///
/// fn issue(id: u64, slug: &str, project: &str, status: Status, who: Option<&str>) -> Issue {
///     Issue {
///         id,
///         slug: slug.into(),
///         project: project.into(),
///         status,
///         assignee: who.map(String::from),
///         labels: vec!["bug".into()],
///     }
/// }
///
/// let mut issues: Table<Issue> = Table::new();
/// issues.insert(issue(1, "crash", "core", Status::Open, Some("ana")));
/// issues.insert(issue(2, "typo", "docs", Status::Closed, None));
/// issues.insert(issue(3, "leak", "core", Status::Closed, Some("ben")));
/// issues.insert(issue(4, "hang", "core", Status::Open, None));
///
/// let ids = |rows: ferro::table::IndexIter<Issue>| rows.map(|row| row.id).collect::<Vec<_>>();
///
/// assert_eq!(ids(issues.by_status(&Status::Open)), [1, 4]);
/// assert_eq!(ids(issues.by_status_after(&Status::Open, &1)), [4]);
/// assert_eq!(ids(issues.by_assignee_some("ben")), [3]);
/// assert_eq!(ids(issues.by_assignee(&None)), [2, 4]);
/// assert_eq!(ids(issues.by_labels("bug")), [1, 2, 3, 4]);
/// assert_eq!(ids(issues.by_project_status(&("core".to_string(), Status::Closed))), [3]);
/// assert_eq!(ids(issues.by_project_status_prefix("core")), [1, 4, 3]);
/// assert_eq!(issues.by_status_counts(), [(Status::Open, 2), (Status::Closed, 2)]);
///
/// assert_eq!(issues.by_slug("leak").map(|row| row.id), Some(3));
/// assert_eq!(issues.key_by_slug("typo"), Some(2));
/// assert!(issues.try_insert(issue(5, "crash", "web", Status::Open, None)).is_err());
///
/// let find = issues.find();
/// let open_core: Vec<u64> = find
///     .by_project("core")
///     .and(find.by_status(&Status::Open))
///     .keys()
///     .collect();
/// assert_eq!(open_core, [1, 4]);
/// ```
///
/// A covering index answers from its entries alone:
///
/// ```
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Record, Clone)]
/// #[index(customer, first, cover(first, total))]
/// struct Booking {
///     #[key]
///     id: u64,
///     #[index(cover(nights, total))]
///     customer: u32,
///     #[unique(cover(total))]
///     reference: String,
///     first: u32,
///     nights: u32,
///     total: i64,
/// }
///
/// let mut bookings: Table<Booking> = Table::new();
/// for (id, first, nights) in [(1, 30, 2), (2, 10, 1), (3, 20, 4)] {
///     let reference = format!("R{id}");
///     let total = i64::from(nights) * 100;
///     bookings.insert(Booking { id, customer: 7, reference, first, nights, total });
/// }
///
/// let newest: Vec<(u64, (u32, i64))> = bookings.by_customer(&7).covered().rev().collect();
/// assert_eq!(newest, [(3, (4, 400)), (2, (1, 100)), (1, (2, 200))]);
///
/// let by_date: Vec<(u64, (u32, i64))> =
///     bookings.by_customer_first_prefix(&7).covered().collect();
/// assert_eq!(by_date, [(2, (10, 100)), (3, (20, 400)), (1, (30, 200))]);
///
/// assert_eq!(bookings.covered_by_reference("R3"), Some((3, 400)));
/// ```
///
/// A unique index lists its rows by range, and a composite one by prefix:
///
/// ```
/// use std::ops::Bound;
///
/// use ferro::{Data, Record, Table};
///
/// #[derive(Data, Record, Clone)]
/// #[unique(room, night, cover(guest))]
/// struct Stay {
///     #[key]
///     id: u64,
///     #[unique]
///     reference: String,
///     #[unique]
///     voucher: Option<u32>,
///     room: u16,
///     night: u32,
///     guest: String,
/// }
///
/// let mut stays: Table<Stay> = Table::new();
/// let plan = [(1, 101, 3, None), (2, 102, 1, Some(9)), (3, 101, 1, None), (4, 101, 2, Some(4))];
/// for (id, room, night, voucher) in plan {
///     let reference = format!("S{id}");
///     let guest = format!("guest {id}");
///     stays.insert(Stay { id, reference, voucher, room, night, guest });
/// }
///
/// let ids = |rows: ferro::table::IndexIter<Stay, String>| {
///     rows.map(|row| row.id).collect::<Vec<_>>()
/// };
/// assert_eq!(ids(stays.by_room_night_prefix(&101)), [3, 4, 1]);
///
/// let from_two: Vec<(u64, String)> =
///     stays.by_room_night_range((101, 2)..=(101, u32::MAX)).covered().collect();
/// assert_eq!(from_two, [(4, "guest 4".to_string()), (1, "guest 1".to_string())]);
///
/// let latest: Vec<u64> = stays
///     .by_reference_range::<str, _>((Bound::Included("S2"), Bound::Unbounded))
///     .rev()
///     .map(|stay| stay.id)
///     .collect();
/// assert_eq!(latest, [4, 3, 2]);
///
/// let vouchers: Vec<u64> = stays.by_voucher_range(..10).keys().collect();
/// assert_eq!(vouchers, [4, 2]);
/// ```
#[proc_macro_derive(Record, attributes(key, index, unique, ferro))]
pub fn derive_record(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_record(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(input: DeriveInput) -> String {
        match expand_record(input) {
            Ok(_) => {
                panic!("the derive accepted a field it does not store as indexed or as the key")
            }
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn an_index_on_a_skipped_field_is_refused_whatever_its_kind() {
        let inputs: [DeriveInput; 5] = [
            syn::parse_quote! {
                struct Row { #[key] id: u64, #[index] #[ferro(skip)] ghost: u32 }
            },
            syn::parse_quote! {
                struct Row { #[key] id: u64, #[index(each)] #[ferro(skip)] ghost: Vec<u32> }
            },
            syn::parse_quote! {
                struct Row { #[key] id: u64, #[unique] #[ferro(skip)] ghost: u32 }
            },
            syn::parse_quote! {
                #[index(room, ghost)]
                struct Row { #[key] id: u64, room: u32, #[ferro(skip)] ghost: u32 }
            },
            syn::parse_quote! {
                #[unique(ghost, room)]
                struct Row { #[key] id: u64, room: u32, #[ferro(skip)] ghost: u32 }
            },
        ];
        for input in inputs {
            let message = refused(input);
            assert!(
                message.starts_with("`ghost` is not stored, so it cannot be indexed"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_skipped_key_is_refused() {
        let inputs: [DeriveInput; 2] = [
            syn::parse_quote! {
                struct Row { #[key] #[ferro(skip)] ghost: u64, #[index] room: u32 }
            },
            syn::parse_quote! {
                struct Row { #[ferro(skip)] #[key] ghost: u64, room: u32 }
            },
        ];
        for input in inputs {
            let message = refused(input);
            assert!(
                message.starts_with("`ghost` is not stored, so it cannot be the key"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_skipped_field_beside_an_index_is_accepted() {
        let input: DeriveInput = syn::parse_quote! {
            struct Row { #[key] id: u64, #[index] room: u32, #[ferro(skip)] ghost: u32 }
        };
        assert!(expand_record(input).is_ok());
    }

    fn data_refused(input: DeriveInput) -> String {
        match expand_data(input) {
            Ok(_) => panic!("the derive accepted a tag that a removed name had"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_field_or_variant_with_the_tag_of_a_removed_one_is_refused() {
        assert_eq!(field_tag("title_score"), field_tag("flags"));
        assert_eq!(field_tag("RefundPremium"), field_tag("Tax"));
        let inputs: [(DeriveInput, &str); 6] = [
            (
                syn::parse_quote! {
                    #[ferro(removed("title_score"))]
                    struct After { id: u32, flags: u32 }
                },
                "field `flags` has the same tag",
            ),
            (
                syn::parse_quote! {
                    #[ferro(removed("old", "title_score"))]
                    struct After(u32, #[ferro(rename = "flags")] u32);
                },
                "field `flags` has the same tag",
            ),
            (
                syn::parse_quote! {
                    #[ferro(removed("RefundPremium"))]
                    enum State { Open, Tax(u32) }
                },
                "variant `Tax` has the same tag",
            ),
            (
                syn::parse_quote! {
                    #[ferro(removed("RefundPremium"))]
                    enum State { Open, #[ferro(rename = "Tax")] Levy(u32) }
                },
                "variant `Tax` has the same tag",
            ),
            (
                syn::parse_quote! {
                    enum State { Open, #[ferro(removed("title_score"))] Paid { flags: u32 } }
                },
                "field `flags` has the same tag",
            ),
            (
                syn::parse_quote! {
                    #[ferro(removed("note"))]
                    struct After { note: String }
                },
                "field `note` is listed as removed",
            ),
        ];
        for (input, expected) in inputs {
            let message = data_refused(input);
            assert!(message.starts_with(expected), "{message}");
        }
    }

    #[test]
    fn a_removed_list_where_it_means_nothing_is_refused() {
        let inputs: [(DeriveInput, &str); 3] = [
            (
                syn::parse_quote! {
                    #[ferro(transparent, removed("old"))]
                    struct Cents(i64);
                },
                "a transparent struct stores its field without a tag",
            ),
            (
                syn::parse_quote! {
                    struct After { #[ferro(removed("old"))] id: u32 }
                },
                "removed lists the names",
            ),
            (
                syn::parse_quote! {
                    #[ferro(removed())]
                    struct After { id: u32 }
                },
                "removed needs at least one name",
            ),
        ];
        for (input, expected) in inputs {
            let message = data_refused(input);
            assert!(message.starts_with(expected), "{message}");
        }
    }

    #[test]
    fn removed_names_whose_tags_no_field_or_variant_has_are_accepted() {
        let inputs: [DeriveInput; 3] = [
            syn::parse_quote! {
                #[ferro(removed("title_score", "legacy"))]
                struct After { id: u32, #[ferro(rename = "flag_bits")] flags: u32 }
            },
            syn::parse_quote! {
                #[ferro(removed("RefundPremium"))]
                enum State {
                    Open,
                    #[ferro(rename = "Levy", removed("title_score"))]
                    Tax { amount: u32 },
                }
            },
            syn::parse_quote! {
                #[ferro(removed("old"))]
                struct Empty;
            },
        ];
        for input in inputs {
            if let Err(error) = expand_data(input) {
                panic!("{error}");
            }
        }
    }
}
