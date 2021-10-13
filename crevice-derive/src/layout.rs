use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{parse_quote, Data, DeriveInput, Fields, Ident, Path};

pub struct EmitOptions {
    /// The Rust-friendly name of the layout, like Std140.
    pub layout_name: Ident,

    /// The minimum alignment for a struct in this layout.
    pub min_struct_alignment: usize,

    /// The fully-qualified path to the Crevice module containing everything for
    /// this layout.
    pub mod_path: Path,

    /// The fully-qualified path to the trait defining a type in this layout.
    pub trait_path: Path,

    /// The fully-qualified path to the trait implemented for types that can be
    /// converted into this layout, like AsStd140.
    pub as_trait_path: Path,

    /// The name of the associated type contained in AsTrait.
    pub as_trait_assoc: Ident,

    /// The name of the method used to convert from AsTrait to Trait.
    pub as_trait_method: Ident,

    // The name of the method used to convert from Trait to AsTrait.
    pub from_trait_method: Ident,
}

impl EmitOptions {
    pub fn new(
        layout_name: &'static str,
        mod_name: &'static str,
        min_struct_alignment: usize,
    ) -> Self {
        let mod_name = Ident::new(mod_name, Span::call_site());
        let layout_name = Ident::new(layout_name, Span::call_site());

        let mod_path = parse_quote!(::crevice::#mod_name);
        let trait_path = parse_quote!(#mod_path::#layout_name);

        let as_trait_name = format_ident!("As{}", layout_name);
        let as_trait_path = parse_quote!(#mod_path::#as_trait_name);
        let as_trait_assoc = format_ident!("{}Type", layout_name);
        let as_trait_method = format_ident!("as_{}", mod_name);
        let from_trait_method = format_ident!("from_{}", mod_name);

        Self {
            layout_name,
            min_struct_alignment,

            mod_path,
            trait_path,
            as_trait_path,
            as_trait_assoc,
            as_trait_method,
            from_trait_method,
        }
    }

    pub fn emit(&self, input: DeriveInput) -> TokenStream {
        let min_struct_alignment = self.min_struct_alignment;
        let layout_name = &self.layout_name;
        let mod_path = &self.mod_path;
        let trait_path = &self.trait_path;
        let as_trait_path = &self.as_trait_path;
        let as_trait_assoc = &self.as_trait_assoc;
        let as_trait_method = &self.as_trait_method;
        let from_trait_method = &self.from_trait_method;

        let visibility = input.vis;

        let name = input.ident;
        let generated_name = format_ident!("{}{}", layout_name, name);

        let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

        let fields = match &input.data {
            Data::Struct(data) => match &data.fields {
                Fields::Named(fields) => fields,
                Fields::Unnamed(_) => panic!("Tuple structs are not supported"),
                Fields::Unit => panic!("Unit structs are not supported"),
            },
            Data::Enum(_) | Data::Union(_) => panic!("Only structs are supported"),
        };

        // Generate the names we'll use for calculating alignment of each field.
        // Each name will turn into a const fn that's invoked to compute the
        // size of a padding array before each field.
        let align_names: Vec<_> = fields
            .named
            .iter()
            .map(|field| {
                format_ident!(
                    "_{}__{}__{}__align",
                    name,
                    field.ident.as_ref().unwrap(),
                    layout_name
                )
            })
            .collect();

        // Generate one function per field that is used to apply alignment
        // padding. Each function invokes all previous functions to calculate
        // the total offset into the struct for the current field, then aligns
        // up to the nearest multiple of alignment.
        let alignment_calculators: Vec<_> = fields
            .named
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let align_name = &align_names[index];

                let offset_accumulation =
                    fields
                        .named
                        .iter()
                        .zip(&align_names)
                        .take(index)
                        .map(|(field, align_name)| {
                            let field_ty = &field.ty;
                            quote! {
                                offset += #align_name();
                                offset += ::core::mem::size_of::<<#field_ty as #as_trait_path>::#as_trait_assoc>();
                            }
                        });

                let pad_at_end = index
                    .checked_sub(1)
                    .map_or(quote!{0usize}, |prev_index|{
                        let field = &fields.named[prev_index];
                        let field_ty = &field.ty;
                        quote! {
                            if <<#field_ty as #as_trait_path>::#as_trait_assoc as #mod_path::#layout_name>::PAD_AT_END {
                                <<#field_ty as #as_trait_path>::#as_trait_assoc as #mod_path::#layout_name>::ALIGNMENT
                            }
                            else {
                                0usize
                            }
                        }
                    });

                let field_ty = &field.ty;

                quote! {
                    #[allow(non_snake_case)]
                    pub const fn #align_name() -> usize {
                        let mut offset = 0;
                        #( #offset_accumulation )*

                        ::crevice::internal::align_offset(
                            offset,
                            ::crevice::internal::max(
                                <<#field_ty as #as_trait_path>::#as_trait_assoc as #mod_path::#layout_name>::ALIGNMENT,
                                #pad_at_end
                            )
                        )
                    }
                }
            })
            .collect();

        // Generate the struct fields that will be present in the generated
        // struct. Each field in the original struct turns into two fields in
        // the generated struct:
        //
        // * Alignment, a byte array whose size is computed from #align_name().
        // * Data, the layout-specific version of the original field.
        let generated_fields: Vec<_> = fields
            .named
            .iter()
            .zip(&align_names)
            .map(|(field, align_name)| {
                let field_ty = &field.ty;
                let field_name = field.ident.as_ref().unwrap();

                quote! {
                    #align_name: [u8; #align_name()],
                    #field_name: <#field_ty as #as_trait_path>::#as_trait_assoc,
                }
            })
            .collect();

        // Generate an initializer for each field in the original struct.
        // Alignment fields are filled in with zeroes using struct update
        // syntax.
        let field_initializers: Vec<_> = fields
            .named
            .iter()
            .map(|field| {
                let field_name = field.ident.as_ref().unwrap();

                quote!(#field_name: self.#field_name.#as_trait_method())
            })
            .collect();

        let field_unwrappers: Vec<_> = fields
            .named
            .iter()
            .map(|field|{
                let field_name = field.ident.as_ref().unwrap();
                let field_ty = &field.ty;
                quote!(#field_name: <#field_ty as #as_trait_path>::#from_trait_method(value.#field_name))
            })
            .collect();

        // This fold builds up an expression that finds the maximum alignment out of
        // all of the fields in the struct. For this struct:
        //
        // struct Foo { a: ty1, b: ty2 }
        //
        // ...we should generate an expression like this:
        //
        // max(ty2_align, max(ty1_align, min_align))
        let struct_alignment = fields.named.iter().fold(
            quote!(#min_struct_alignment),
            |last, field| {
                let field_ty = &field.ty;

                quote! {
                    ::crevice::internal::max(
                        <<#field_ty as #as_trait_path>::#as_trait_assoc as #trait_path>::ALIGNMENT,
                        #last,
                    )
                }
            },
        );

        // For testing purposes, we can optionally generate type layout
        // information using the type-layout crate.
        let type_layout_derive = if cfg!(feature = "test_type_layout") {
            quote!(#[derive(::type_layout::TypeLayout)])
        } else {
            quote!()
        };

        quote! {
            #( #alignment_calculators )*

            #[derive(Debug, Clone, Copy)]
            #type_layout_derive
            #[repr(C)]
            #[allow(non_snake_case)]
            #visibility struct #generated_name #ty_generics #where_clause {
                #( #generated_fields )*
            }

            unsafe impl #impl_generics ::crevice::internal::bytemuck::Zeroable for #generated_name #ty_generics #where_clause {}
            unsafe impl #impl_generics ::crevice::internal::bytemuck::Pod for #generated_name #ty_generics #where_clause {}

            unsafe impl #impl_generics #mod_path::#layout_name for #generated_name #ty_generics #where_clause {
                const ALIGNMENT: usize = #struct_alignment;
                const PAD_AT_END: bool = true;
            }

            impl #impl_generics #as_trait_path for #name #ty_generics #where_clause {
                type #as_trait_assoc = #generated_name;

                fn #as_trait_method(&self) -> Self::#as_trait_assoc {
                    Self::#as_trait_assoc {
                        #( #field_initializers, )*

                        ..::crevice::internal::bytemuck::Zeroable::zeroed()
                    }
                }

                fn #from_trait_method(value: Self::#as_trait_assoc) -> Self {
                    Self {
                        #( #field_unwrappers, )*
                    }
                }
            }
        }
    }
}
